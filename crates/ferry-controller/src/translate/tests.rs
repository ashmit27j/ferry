//! Translator tests, modelled on the Gateway API conformance manifests.

use std::sync::Arc;

use ferry_config::{PathMatchKind, SnapshotVersion, UpstreamProtocol};
use proptest::prelude::*;

use super::backend::tests::{self_signed, tls_secret};
use super::*;
use crate::world::Fleet;

const CONTROLLER: &str = "gateway.ferry.dev/controller";

fn settings() -> Settings {
    Settings {
        controller_name: CONTROLLER.into(),
        proxy_protocol: false,
    }
}

/// Namespaces, the class, three Gateways and two backends, as in the
/// conformance suite's base manifests.
const BASE: &str = r#"
apiVersion: v1
kind: Namespace
metadata: { name: infra, labels: { gateway-conformance: infra } }
---
kind: Namespace
metadata: { name: web, labels: { gateway-conformance: backend } }
---
kind: Namespace
metadata: { name: other }
---
kind: GatewayClass
metadata: { name: ferry, generation: 1 }
spec: { controllerName: gateway.ferry.dev/controller }
---
kind: GatewayClass
metadata: { name: someone-else }
spec: { controllerName: example.com/other }
---
kind: Gateway
metadata: { name: same-namespace, namespace: infra, generation: 2 }
spec:
  gatewayClassName: ferry
  listeners:
    - { name: http, port: 80, protocol: HTTP, allowedRoutes: { namespaces: { from: Same } } }
---
kind: Gateway
metadata: { name: all-namespaces, namespace: infra }
spec:
  gatewayClassName: ferry
  listeners:
    - { name: http, port: 80, protocol: HTTP, allowedRoutes: { namespaces: { from: All } } }
---
kind: Gateway
metadata: { name: backend-namespaces, namespace: infra }
spec:
  gatewayClassName: ferry
  listeners:
    - name: http
      port: 80
      protocol: HTTP
      allowedRoutes:
        namespaces:
          from: Selector
          selector: { matchLabels: { gateway-conformance: backend } }
---
kind: Gateway
metadata: { name: foreign, namespace: infra }
spec:
  gatewayClassName: someone-else
  listeners: [{ name: http, port: 80, protocol: HTTP }]
---
kind: Service
metadata: { name: infra-backend-v1, namespace: infra }
spec:
  ports:
    - { name: first-port, port: 8080, targetPort: 3000 }
    - { name: second-port, port: 8081, targetPort: 3001, appProtocol: kubernetes.io/h2c }
---
kind: EndpointSlice
metadata: { name: v1-a, namespace: infra, labels: { kubernetes.io/service-name: infra-backend-v1 } }
addressType: IPv4
ports: [{ name: first-port, port: 3000 }, { name: second-port, port: 3001 }]
endpoints:
  - { addresses: [10.0.0.1], conditions: { ready: true } }
  - { addresses: [10.0.0.2] }
  - { addresses: [10.0.0.9], conditions: { ready: false } }
---
kind: EndpointSlice
metadata: { name: v1-b, namespace: infra, labels: { kubernetes.io/service-name: infra-backend-v1 } }
addressType: IPv4
ports: [{ name: first-port, port: 3000 }]
endpoints: [{ addresses: [10.0.0.1, 10.0.0.3] }]
---
kind: Service
metadata: { name: web-backend, namespace: web }
spec: { ports: [{ port: 8080, targetPort: 3000 }] }
---
kind: EndpointSlice
metadata: { name: wb, namespace: web, labels: { kubernetes.io/service-name: web-backend } }
addressType: IPv4
ports: [{ port: 3000 }]
endpoints: [{ addresses: [10.1.0.1] }]
"#;

fn world(extra: &str) -> World {
    World::from_manifests(&format!("{BASE}\n---\n{extra}")).unwrap()
}

fn run(w: &World) -> Translation {
    let t = translate(w, &settings());
    for (k, g) in &t.gateways {
        g.config
            .clone()
            .into_snapshot(SnapshotVersion(1))
            .unwrap_or_else(|e| panic!("{k}: config does not validate: {e}"));
        assert!(g.valid, "{k}");
    }
    t
}

fn gw<'a>(t: &'a Translation, name: &str) -> &'a GatewayOutput {
    &t.gateways[&ObjKey::new("infra", name)]
}

fn cond<'a>(conds: &'a [Cond], type_: &str) -> &'a Cond {
    conds
        .iter()
        .find(|c| c.type_ == type_)
        .unwrap_or_else(|| panic!("no {type_} in {conds:?}"))
}

fn route_parents<'a>(
    t: &'a Translation,
    kind: RouteKind,
    ns: &str,
    name: &str,
) -> &'a [ParentStatus] {
    &t.routes[&(kind, ObjKey::new(ns, name))].parents
}

fn accepted(p: &ParentStatus) -> (bool, &str) {
    let c = cond(&p.conditions, "Accepted");
    (c.status, c.reason.as_str())
}

#[test]
fn only_our_classes_and_their_gateways_are_handled() {
    let t = run(&world(""));
    assert_eq!(t.classes.keys().collect::<Vec<_>>(), ["ferry"]);
    assert!(cond(&t.classes["ferry"].conditions, "Accepted").status);
    assert_eq!(t.gateways.len(), 3, "the foreign Gateway is not ours");
    let g = gw(&t, "same-namespace");
    assert_eq!(g.generation, 2);
    let a = cond(&g.conditions, "Accepted");
    assert_eq!((a.status, a.reason.as_str()), (true, "Accepted"));
    let p = cond(&g.conditions, "Programmed");
    assert_eq!(
        (p.status, p.reason.as_str()),
        (false, "Pending"),
        "no proxies yet"
    );
    let l = &g.listeners[0];
    assert_eq!(l.supported_kinds, [RouteKind::Http, RouteKind::Grpc]);
    assert!(cond(&l.conditions, "Programmed").status);
    assert!(!cond(&l.conditions, "Conflicted").status);
}

#[test]
fn ports_below_1024_move_up_and_keep_their_advertised_port() {
    let t = run(&world(""));
    let g = gw(&t, "same-namespace");
    assert_eq!(
        g.ports,
        [ProxyPort {
            name: "http-80".into(),
            port: 80,
            container_port: 10_080
        }]
    );
    let l = &g.config.listeners[0];
    assert_eq!(l.address.port(), 10_080);
    assert_eq!(l.advertised_port, Some(80));
    assert_eq!(container_port(8080), 8080);
}

#[test]
fn programmed_once_proxies_are_ready_with_an_address() {
    let mut w = world("");
    w.fleets.insert(
        ObjKey::new("infra", "same-namespace"),
        Fleet {
            ready_replicas: 2,
            addresses: Vec::new(),
        },
    );
    let t = run(&w);
    assert_eq!(
        cond(&gw(&t, "same-namespace").conditions, "Programmed").reason,
        "AddressNotAssigned"
    );
    w.fleets.insert(
        ObjKey::new("infra", "same-namespace"),
        Fleet {
            ready_replicas: 2,
            addresses: vec![("IPAddress".into(), "172.18.0.10".into())],
        },
    );
    let t = run(&w);
    let g = gw(&t, "same-namespace");
    assert!(cond(&g.conditions, "Programmed").status);
    assert_eq!(
        g.addresses,
        [("IPAddress".to_owned(), "172.18.0.10".to_owned())]
    );
}

#[test]
fn simple_route_reaches_ready_endpoints() {
    let t = run(&world(
        r"
kind: HTTPRoute
metadata: { name: r, namespace: infra, generation: 3 }
spec:
  parentRefs: [{ name: same-namespace }]
  rules: [{ backendRefs: [{ name: infra-backend-v1, port: 8080 }] }]
",
    ));
    let parents = route_parents(&t, RouteKind::Http, "infra", "r");
    assert_eq!(parents.len(), 1);
    let p = &parents[0];
    assert_eq!(
        (p.group.as_str(), p.kind.as_str(), p.namespace.as_str()),
        (GROUP, "Gateway", "infra")
    );
    assert_eq!(accepted(p), (true, "Accepted"));
    assert!(cond(&p.conditions, "ResolvedRefs").status);
    assert_eq!(
        t.routes[&(RouteKind::Http, ObjKey::new("infra", "r"))].generation,
        3
    );

    let g = gw(&t, "same-namespace");
    assert_eq!(g.listeners[0].attached_routes, 1);
    let route = &g.config.listeners[0].routes[0];
    assert_eq!(route.name, "httproute_infra_r");
    assert!(route.hostnames.is_empty());
    assert_eq!(
        route.rules[0].backends[0].cluster,
        "infra_infra-backend-v1_8080"
    );
    let c = &g.config.clusters[0];
    let eps: Vec<String> = c.endpoints.iter().map(|e| e.address.to_string()).collect();
    assert_eq!(
        eps,
        ["10.0.0.1:3000", "10.0.0.2:3000", "10.0.0.3:3000"],
        "deduplicated, not-ready dropped"
    );
    assert_eq!(c.protocol, UpstreamProtocol::Http1);
}

#[test]
fn h2c_app_protocol_selects_http2() {
    let t = run(&world(
        r"
kind: HTTPRoute
metadata: { name: r, namespace: infra }
spec:
  parentRefs: [{ name: same-namespace }]
  rules: [{ backendRefs: [{ name: infra-backend-v1, port: 8081 }] }]
",
    ));
    let c = &gw(&t, "same-namespace").config.clusters[0];
    assert_eq!(c.name, "infra_infra-backend-v1_8081_h2");
    assert_eq!(c.protocol, UpstreamProtocol::Http2);
    assert_eq!(c.endpoints[0].address.port(), 3001);
}

#[test]
fn allowed_routes_namespaces() {
    let t = run(&world(
        r"
kind: HTTPRoute
metadata: { name: r, namespace: web }
spec:
  parentRefs:
    - { name: same-namespace, namespace: infra }
    - { name: all-namespaces, namespace: infra }
    - { name: backend-namespaces, namespace: infra }
  rules: [{ backendRefs: [{ name: web-backend, port: 8080 }] }]
---
kind: HTTPRoute
metadata: { name: r, namespace: other }
spec:
  parentRefs: [{ name: backend-namespaces, namespace: infra }]
  rules: [{ backendRefs: [{ name: x, port: 8080 }] }]
",
    ));
    let parents = route_parents(&t, RouteKind::Http, "web", "r");
    let got: Vec<_> = parents
        .iter()
        .map(|p| (p.name.as_str(), accepted(p)))
        .collect();
    assert_eq!(
        got,
        [
            ("same-namespace", (false, "NotAllowedByListeners")),
            ("all-namespaces", (true, "Accepted")),
            ("backend-namespaces", (true, "Accepted")),
        ]
    );
    let other = route_parents(&t, RouteKind::Http, "other", "r");
    assert_eq!(accepted(&other[0]), (false, "NotAllowedByListeners"));
    assert_eq!(gw(&t, "same-namespace").listeners[0].attached_routes, 0);
}

#[test]
fn parents_of_other_controllers_get_no_status() {
    let t = run(&world(
        r"
kind: HTTPRoute
metadata: { name: r, namespace: infra }
spec:
  parentRefs: [{ name: foreign }, { name: missing }]
  rules: [{ backendRefs: [{ name: infra-backend-v1, port: 8080 }] }]
",
    ));
    assert!(route_parents(&t, RouteKind::Http, "infra", "r").is_empty());
}

#[test]
fn section_names_and_hostnames() {
    let t = run(&world(
        r#"
kind: Gateway
metadata: { name: hosts, namespace: infra }
spec:
  gatewayClassName: ferry
  listeners:
    - { name: bar, port: 80, protocol: HTTP, hostname: bar.com }
    - { name: foo-bar, port: 80, protocol: HTTP, hostname: foo.bar.com }
    - { name: wild-bar, port: 80, protocol: HTTP, hostname: "*.bar.com" }
---
kind: HTTPRoute
metadata: { name: to-bar, namespace: infra }
spec:
  parentRefs: [{ name: hosts, sectionName: bar }]
  rules: [{ backendRefs: [{ name: infra-backend-v1, port: 8080 }] }]
---
kind: HTTPRoute
metadata: { name: to-wild, namespace: infra }
spec:
  parentRefs: [{ name: hosts, sectionName: wild-bar }]
  hostnames: [a.bar.com, "*.x.bar.com", other.org]
  rules: [{ backendRefs: [{ name: infra-backend-v1, port: 8080 }] }]
---
kind: HTTPRoute
metadata: { name: no-host-match, namespace: infra }
spec:
  parentRefs: [{ name: hosts }]
  hostnames: [nothing.org]
  rules: [{ backendRefs: [{ name: infra-backend-v1, port: 8080 }] }]
---
kind: HTTPRoute
metadata: { name: no-section, namespace: infra }
spec:
  parentRefs: [{ name: hosts, sectionName: nope }]
  rules: [{ backendRefs: [{ name: infra-backend-v1, port: 8080 }] }]
---
kind: HTTPRoute
metadata: { name: wrong-port, namespace: infra }
spec:
  parentRefs: [{ name: hosts, port: 8080 }]
  rules: [{ backendRefs: [{ name: infra-backend-v1, port: 8080 }] }]
"#,
    ));
    let a = |name| {
        accepted(&route_parents(&t, RouteKind::Http, "infra", name)[0])
            .1
            .to_owned()
    };
    assert_eq!(a("to-bar"), "Accepted");
    assert_eq!(a("to-wild"), "Accepted");
    assert_eq!(a("no-host-match"), "NoMatchingListenerHostname");
    assert_eq!(a("no-section"), "NoMatchingParent");
    assert_eq!(a("wrong-port"), "NoMatchingParent");

    let g = gw(&t, "hosts");
    assert_eq!(g.config.listeners.len(), 1, "one Ferry listener per port");
    let hosts: Vec<(&str, &[String])> = g.config.listeners[0]
        .routes
        .iter()
        .map(|r| (r.name.as_str(), r.hostnames.as_slice()))
        .collect();
    assert_eq!(
        hosts,
        [
            ("httproute_infra_to-bar", &["bar.com".to_owned()][..]),
            (
                "httproute_infra_to-wild",
                &["a.bar.com".to_owned(), "*.x.bar.com".to_owned()][..]
            ),
        ]
    );
    let attached: Vec<i32> = g.listeners.iter().map(|l| l.attached_routes).collect();
    assert_eq!(attached, [1, 0, 1]);
}

#[test]
fn invalid_backends_answer_500_and_explain_why() {
    let t = run(&world(
        r"
kind: HTTPRoute
metadata: { name: r, namespace: infra }
spec:
  parentRefs: [{ name: same-namespace }]
  rules:
    - matches: [{ path: { type: Exact, value: /kind } }]
      backendRefs: [{ group: unknownkind.example.com, kind: NonExistent, name: infra-backend-v1, port: 8080 }]
    - matches: [{ path: { type: Exact, value: /missing } }]
      backendRefs: [{ name: nope, port: 8080 }]
    - matches: [{ path: { type: Exact, value: /mixed } }]
      backendRefs: [{ name: infra-backend-v1, port: 8080, weight: 3 }, { name: nope, port: 8080 }]
    - matches: [{ path: { type: Exact, value: /none } }]
    - matches: [{ path: { type: Exact, value: /port } }]
      backendRefs: [{ name: infra-backend-v1, port: 9999 }]
",
    ));
    let p = &route_parents(&t, RouteKind::Http, "infra", "r")[0];
    assert_eq!(
        accepted(p),
        (true, "Accepted"),
        "invalid refs do not stop attachment"
    );
    let refs = cond(&p.conditions, "ResolvedRefs");
    assert_eq!((refs.status, refs.reason.as_str()), (false, "InvalidKind"));
    let rules = &gw(&t, "same-namespace").config.listeners[0].routes[0].rules;
    let shape: Vec<(usize, u32)> = rules
        .iter()
        .map(|r| (r.backends.len(), r.invalid_backend_weight))
        .collect();
    assert_eq!(shape, [(0, 1), (0, 1), (1, 1), (0, 1), (0, 1)]);
}

#[test]
fn cross_namespace_backends_need_a_reference_grant() {
    let route = r"
kind: HTTPRoute
metadata: { name: r, namespace: infra }
spec:
  parentRefs: [{ name: same-namespace }]
  rules: [{ backendRefs: [{ name: web-backend, namespace: web, port: 8080 }] }]
";
    let t = run(&world(route));
    let refs = cond(
        &route_parents(&t, RouteKind::Http, "infra", "r")[0].conditions,
        "ResolvedRefs",
    )
    .clone();
    assert_eq!(
        (refs.status, refs.reason.as_str()),
        (false, "RefNotPermitted")
    );

    let granted = format!(
        "{route}---\n{}",
        r"
kind: ReferenceGrant
metadata: { name: g, namespace: web }
spec:
  from: [{ group: gateway.networking.k8s.io, kind: HTTPRoute, namespace: infra }]
  to: [{ group: '', kind: Service, name: web-backend }]
"
    );
    let t = run(&world(&granted));
    assert!(
        cond(
            &route_parents(&t, RouteKind::Http, "infra", "r")[0].conditions,
            "ResolvedRefs"
        )
        .status
    );
    let grpc_only = granted.replace("kind: HTTPRoute, namespace", "kind: GRPCRoute, namespace");
    let t = run(&world(&grpc_only));
    assert!(
        !cond(
            &route_parents(&t, RouteKind::Http, "infra", "r")[0].conditions,
            "ResolvedRefs"
        )
        .status
    );
}

#[test]
fn unsupported_protocols_and_conflicts() {
    let t = run(&world(
        r#"
kind: Gateway
metadata: { name: only-bad, namespace: infra }
spec:
  gatewayClassName: ferry
  listeners: [{ name: invalid, port: 1111, protocol: INVALID }]
---
kind: Gateway
metadata: { name: mixed, namespace: infra }
spec:
  gatewayClassName: ferry
  listeners:
    - { name: http, port: 80, protocol: HTTP }
    - { name: invalid, port: 1111, protocol: TCP }
    - { name: dup, port: 80, protocol: HTTP }
    - { name: tls-on-80, port: 80, protocol: HTTPS, tls: { certificateRefs: [{ name: c }] } }
    - { name: admin, port: 9001, protocol: HTTP }
    - { name: hidden, port: 19001, protocol: HTTP }
"#,
    ));
    let bad = gw(&t, "only-bad");
    let a = cond(&bad.conditions, "Accepted");
    assert_eq!((a.status, a.reason.as_str()), (false, "ListenersNotValid"));
    let l = &bad.listeners[0];
    assert_eq!(
        cond(&l.conditions, "Accepted").reason,
        "UnsupportedProtocol"
    );
    assert!(l.supported_kinds.is_empty());
    assert!(bad.config.listeners.is_empty());

    let mixed = gw(&t, "mixed");
    let a = cond(&mixed.conditions, "Accepted");
    assert_eq!((a.status, a.reason.as_str()), (true, "ListenersNotValid"));
    let reasons: Vec<(&str, &str)> = mixed
        .listeners
        .iter()
        .map(|l| {
            let c = cond(&l.conditions, "Conflicted");
            let reason = if c.status {
                c.reason.as_str()
            } else {
                cond(&l.conditions, "Accepted").reason.as_str()
            };
            (l.name.as_str(), reason)
        })
        .collect();
    assert_eq!(
        reasons,
        [
            ("http", "Accepted"),
            ("invalid", "UnsupportedProtocol"),
            ("dup", "HostnameConflict"),
            ("tls-on-80", "ProtocolConflict"),
            ("admin", "Accepted"),
            ("hidden", "PortUnavailable"),
        ]
    );
    let ports: Vec<u16> = mixed.ports.iter().map(|p| p.port).collect();
    assert_eq!(ports, [80, 9001]);
}

fn tls_world(secrets: &str, listeners: &str) -> World {
    let mut w = world(&format!(
        r"
kind: Gateway
metadata: {{ name: tls, namespace: infra }}
spec:
  gatewayClassName: ferry
  listeners:
{listeners}
---
{secrets}
"
    ));
    let (cert, key) = self_signed(&["tls.example"]);
    let mut good = tls_secret(&cert, &key);
    good.metadata.name = Some("good".into());
    good.metadata.namespace = Some("infra".into());
    w.secrets
        .insert(ObjKey::new("infra", "good"), Arc::new(good.clone()));
    good.metadata.namespace = Some("web".into());
    w.secrets.insert(ObjKey::new("web", "good"), Arc::new(good));
    let mut bad = tls_secret("Hello world\n", "Hello world\n");
    bad.metadata.name = Some("malformed".into());
    w.secrets
        .insert(ObjKey::new("infra", "malformed"), Arc::new(bad));
    w
}

#[test]
fn https_listeners_load_certificates_from_secrets() {
    let w = tls_world(
        "",
        r#"
    - { name: https, port: 443, protocol: HTTPS, tls: { certificateRefs: [{ name: good }] } }
    - { name: named, port: 443, protocol: HTTPS, hostname: a.example, tls: { certificateRefs: [{ name: good }] } }
"#,
    );
    let t = run(&w);
    let g = gw(&t, "tls");
    assert!(
        g.listeners
            .iter()
            .all(|l| cond(&l.conditions, "Programmed").status)
    );
    let fl = &g.config.listeners[0];
    assert_eq!(fl.name, "https-443");
    assert_eq!(fl.address.port(), 10_443);
    let certs = &fl.tls.as_ref().unwrap().certificates;
    assert_eq!(certs.len(), 1, "the same Secret twice is one certificate");
    assert!(
        certs[0]
            .cert_pem
            .as_ref()
            .unwrap()
            .contains("BEGIN CERTIFICATE")
    );
}

#[test]
fn bad_certificate_refs_are_reported() {
    let cases = [
        ("{ name: nonexistent }", "InvalidCertificateRef"),
        (
            "{ group: wrong.group.company.io, kind: Secret, name: good }",
            "InvalidCertificateRef",
        ),
        ("{ kind: WrongKind, name: good }", "InvalidCertificateRef"),
        ("{ name: malformed }", "InvalidCertificateRef"),
        ("{ name: good, namespace: web }", "RefNotPermitted"),
    ];
    for (r, reason) in cases {
        let w = tls_world(
            "",
            &format!(
                "    - {{ name: https, port: 443, protocol: HTTPS, tls: {{ certificateRefs: [{r}] }} }}\n"
            ),
        );
        let t = run(&w);
        let l = &gw(&t, "tls").listeners[0];
        let c = cond(&l.conditions, "ResolvedRefs");
        assert_eq!((c.status, c.reason.as_str()), (false, reason), "{r}");
        assert!(!cond(&l.conditions, "Programmed").status, "{r}");
        assert!(
            gw(&t, "tls").config.listeners.is_empty(),
            "{r}: nothing to serve"
        );
    }
    let granted = tls_world(
        r"
kind: ReferenceGrant
metadata: { name: certs, namespace: web }
spec:
  from: [{ group: gateway.networking.k8s.io, kind: Gateway, namespace: infra }]
  to: [{ group: '', kind: Secret }]
",
        "    - { name: https, port: 443, protocol: HTTPS, tls: { certificateRefs: [{ name: good, namespace: web }] } }\n",
    );
    let t = run(&granted);
    assert!(cond(&gw(&t, "tls").listeners[0].conditions, "ResolvedRefs").status);
}

#[test]
fn invalid_route_kinds_leave_the_valid_ones() {
    let t = run(&world(
        r"
kind: Gateway
metadata: { name: kinds, namespace: infra }
spec:
  gatewayClassName: ferry
  listeners:
    - name: http
      port: 80
      protocol: HTTP
      allowedRoutes: { kinds: [{ kind: HTTPRoute }, { kind: InvalidRoute }] }
",
    ));
    let l = &gw(&t, "kinds").listeners[0];
    assert_eq!(l.supported_kinds, [RouteKind::Http]);
    assert_eq!(
        cond(&l.conditions, "ResolvedRefs").reason,
        "InvalidRouteKinds"
    );
}

#[test]
fn older_routes_win_precedence_ties() {
    let t = run(&world(
        r"
kind: HTTPRoute
metadata: { name: a-newer, namespace: infra, creationTimestamp: '2026-02-01T00:00:00Z' }
spec:
  parentRefs: [{ name: same-namespace }]
  rules: [{ backendRefs: [{ name: infra-backend-v1, port: 8080 }] }]
---
kind: HTTPRoute
metadata: { name: z-older, namespace: infra, creationTimestamp: '2026-01-01T00:00:00Z' }
spec:
  parentRefs: [{ name: same-namespace }]
  rules: [{ backendRefs: [{ name: infra-backend-v1, port: 8080 }] }]
",
    ));
    let names: Vec<&str> = gw(&t, "same-namespace").config.listeners[0]
        .routes
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["httproute_infra_z-older", "httproute_infra_a-newer"]
    );
}

#[test]
fn grpc_routes_use_http2_clusters_and_method_paths() {
    let t = run(&world(
        r"
kind: GRPCRoute
metadata: { name: g, namespace: infra }
spec:
  parentRefs: [{ name: same-namespace }]
  rules:
    - matches: [{ method: { service: echo.Echo, method: Ping } }]
      backendRefs: [{ name: infra-backend-v1, port: 8080 }]
",
    ));
    let g = gw(&t, "same-namespace");
    let rule = &g.config.listeners[0].routes[0].rules[0];
    let path = rule.matches[0].path.as_ref().unwrap();
    assert_eq!(
        (path.kind, path.value.as_str()),
        (PathMatchKind::Exact, "/echo.Echo/Ping")
    );
    assert_eq!(g.config.clusters[0].protocol, UpstreamProtocol::Http2);
    assert_eq!(g.config.clusters[0].name, "infra_infra-backend-v1_8080_h2");
}

#[test]
fn unsupported_filters_reject_the_route() {
    let t = run(&world(
        r"
kind: HTTPRoute
metadata: { name: r, namespace: infra }
spec:
  parentRefs: [{ name: same-namespace }]
  rules:
    - filters: [{ type: ExtensionRef, extensionRef: { group: example.com, kind: Thing, name: z } }]
      backendRefs: [{ name: infra-backend-v1, port: 8080 }]
",
    ));
    let p = &route_parents(&t, RouteKind::Http, "infra", "r")[0];
    assert_eq!(accepted(p), (false, "UnsupportedValue"));
    assert!(
        gw(&t, "same-namespace").config.listeners[0]
            .routes
            .is_empty()
    );
}

#[test]
fn ferry_policies_apply_split_and_conflict() {
    let mut w = world(
        r"
kind: HTTPRoute
metadata: { name: r, namespace: infra }
spec:
  parentRefs: [{ name: same-namespace }]
  rules: [{ backendRefs: [{ name: infra-backend-v1, port: 8080 }] }]
---
kind: FerryPolicy
apiVersion: gateway.ferry.dev/v1alpha1
metadata: { name: limit, namespace: infra, creationTimestamp: '2026-01-01T00:00:00Z' }
spec:
  targetRefs: [{ group: gateway.networking.k8s.io, kind: HTTPRoute, name: r }]
  rateLimit: { requests: 100, per: 1s }
  retry: { attempts: 1 }
---
kind: FerryPolicy
metadata: { name: late, namespace: infra, creationTimestamp: '2026-03-01T00:00:00Z' }
spec:
  targetRefs: [{ group: gateway.networking.k8s.io, kind: HTTPRoute, name: r }]
  rateLimit: { requests: 1, per: 1s }
---
kind: FerryPolicy
metadata: { name: lb, namespace: infra }
spec:
  targetRefs: [{ kind: Service, name: infra-backend-v1 }]
  loadBalancer: { kind: LeastRequest }
---
kind: FerryPolicy
metadata: { name: ghost, namespace: infra }
spec:
  targetRefs: [{ kind: Service, name: nope }]
  connectTimeout: 1s
---
kind: FerryPolicy
metadata: { name: wrong, namespace: infra }
spec:
  targetRefs: [{ kind: Service, name: infra-backend-v1 }]
  rateLimit: { requests: 1, per: 1s }
",
    );
    w.fleets.insert(
        ObjKey::new("infra", "same-namespace"),
        Fleet {
            ready_replicas: 3,
            addresses: Vec::new(),
        },
    );
    let t = run(&w);
    let reason = |n: &str| {
        cond(&t.policies[&ObjKey::new("infra", n)].conditions, "Accepted")
            .reason
            .clone()
    };
    assert_eq!(reason("limit"), "Accepted");
    assert_eq!(reason("late"), "Conflicted");
    assert_eq!(reason("lb"), "Accepted");
    assert_eq!(reason("ghost"), "TargetNotFound");
    assert_eq!(reason("wrong"), "Invalid");

    let g = gw(&t, "same-namespace");
    let route = &g.config.listeners[0].routes[0];
    assert_eq!(
        route.rate_limit.as_ref().unwrap().requests,
        34,
        "100/s over 3 replicas"
    );
    assert_eq!(route.rules[0].retry.as_ref().unwrap().attempts, Some(1));
    assert_eq!(
        g.config.clusters[0].lb_policy,
        ferry_config::LbPolicy::LeastRequest
    );
}

#[test]
fn listener_policy_and_proxy_protocol_apply() {
    let w = world(
        r"
kind: FerryPolicy
metadata: { name: edge, namespace: infra }
spec:
  targetRefs: [{ group: gateway.networking.k8s.io, kind: Gateway, name: same-namespace }]
  listener: { maxRequestHeaders: 50, headerReadTimeout: 10s, maxConnections: 5000, tunnelIdleTimeout: 10m }
",
    );
    let t = translate(
        &w,
        &Settings {
            proxy_protocol: true,
            ..settings()
        },
    );
    let l = &gw(&t, "same-namespace").config.listeners[0];
    assert!(l.proxy_protocol);
    assert_eq!(l.limits.as_ref().unwrap().max_request_headers, Some(50));
    assert_eq!(l.limits.as_ref().unwrap().max_connections, Some(5000));
    assert_eq!(
        l.timeouts.as_ref().unwrap().tunnel_idle,
        Some("10m".parse().unwrap())
    );
}

#[test]
fn backend_tls_fails_closed_without_its_secret() {
    let w = world(
        r"
kind: HTTPRoute
metadata: { name: r, namespace: infra }
spec:
  parentRefs: [{ name: same-namespace }]
  rules: [{ backendRefs: [{ name: infra-backend-v1, port: 8080 }] }]
---
kind: FerryPolicy
metadata: { name: tls, namespace: infra }
spec:
  targetRefs: [{ kind: Service, name: infra-backend-v1 }]
  backendTLS: { caSecret: missing }
",
    );
    let t = run(&w);
    let c = &gw(&t, "same-namespace").config.clusters[0];
    assert!(c.endpoints.is_empty(), "no plaintext fallback");
}

#[test]
fn translation_is_deterministic() {
    let w = world(
        r"
kind: HTTPRoute
metadata: { name: r, namespace: web }
spec:
  parentRefs: [{ name: all-namespaces, namespace: infra }, { name: backend-namespaces, namespace: infra }]
  rules: [{ backendRefs: [{ name: web-backend, port: 8080 }] }]
",
    );
    let a = translate(&w, &settings());
    let b = translate(&w, &settings());
    assert_eq!(a.gateways, b.gateways);
    assert_eq!(a.routes, b.routes);
}

prop_compose! {
    fn hostname()(i in 0usize..6) -> String {
        ["a.example", "b.example", "*.example", "x.a.example", "*.a.example", "other.org"][i].to_owned()
    }
}

prop_compose! {
    fn listener_yaml()(port in prop::sample::select(vec![80u16, 443, 8080, 10080]),
                       https in any::<bool>(),
                       host in prop::option::of(hostname()),
                       from in prop::sample::select(vec!["Same", "All"]),
                       good_cert in any::<bool>()) -> String {
        let proto = if https { "HTTPS" } else { "HTTP" };
        let host = host.map(|h| format!(", hostname: \"{h}\"")).unwrap_or_default();
        let secret = if good_cert { "good" } else { "malformed" };
        let tls = if https { format!(", tls: {{ certificateRefs: [{{ name: {secret} }}] }}") } else { String::new() };
        format!("{{ port: {port}, protocol: {proto}{host}{tls}, allowedRoutes: {{ namespaces: {{ from: {from} }} }} }}")
    }
}

prop_compose! {
    fn route_yaml()(hosts in prop::collection::vec(hostname(), 0..3),
                    section in prop::option::of(0usize..4),
                    ns in prop::sample::select(vec!["infra", "web"]),
                    backend in prop::sample::select(vec![
                        "{ name: infra-backend-v1, port: 8080 }",
                        "{ name: infra-backend-v1, port: 8081, weight: 0 }",
                        "{ name: web-backend, namespace: web, port: 8080 }",
                        "{ name: missing, port: 1 }",
                    ]),
                    path in prop::sample::select(vec!["/", "/a", "/a/b"])) -> (String, String, Option<usize>, String, String) {
        let hosts = hosts.iter().map(|h| format!("\"{h}\"")).collect::<Vec<_>>().join(", ");
        (ns.to_owned(), hosts, section, backend.to_owned(), path.to_owned())
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Whatever the Gateways and routes look like, every translated config
    /// validates, every listener reports four conditions, and attached-route
    /// counts match the accepted parents.
    #[test]
    fn every_translation_validates(
        listeners in prop::collection::vec(listener_yaml(), 1..5),
        routes in prop::collection::vec(route_yaml(), 0..6),
    ) {
        let mut yaml = String::from("kind: Gateway\nmetadata: { name: g, namespace: infra }\nspec:\n  gatewayClassName: ferry\n  listeners:\n");
        for (i, l) in listeners.iter().enumerate() {
            yaml.push_str(&format!("    - {{ name: l{i}, {}\n", &l[1..]));
        }
        for (i, (ns, hosts, section, backend, path)) in routes.iter().enumerate() {
            let section = section.map(|s| format!(", sectionName: l{s}")).unwrap_or_default();
            yaml.push_str(&format!(
                "---\nkind: HTTPRoute\nmetadata: {{ name: r{i}, namespace: {ns} }}\nspec:\n  parentRefs: [{{ name: g, namespace: infra{section} }}]\n  hostnames: [{hosts}]\n  rules: [{{ matches: [{{ path: {{ value: \"{path}\" }} }}], backendRefs: [{backend}] }}]\n"
            ));
        }
        let w = tls_world("", "    []\n");
        let mut w2 = World::from_manifests(&format!("{BASE}\n---\n{yaml}")).unwrap();
        w2.secrets = w.secrets.clone();
        let t = translate(&w2, &settings());
        let g = &t.gateways[&ObjKey::new("infra", "g")];
        prop_assert!(g.config.clone().into_snapshot(SnapshotVersion(1)).is_ok(), "{:#?}", g.config);
        prop_assert!(g.listeners.iter().all(|l| l.conditions.len() == 4));
        let accepted_parents = t.routes.values().flat_map(|r| &r.parents).filter(|p| accepted(p).0).count();
        let attached: i32 = g.listeners.iter().map(|l| l.attached_routes).sum();
        prop_assert!(usize::try_from(attached).unwrap() >= accepted_parents);
    }
}
