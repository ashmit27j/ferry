//! Control-plane benchmarks (M4): how long one reconcile pass spends
//! translating the cluster, and serialising a snapshot for proxies.
//!
//! Run with `cargo bench -p ferry-bench --bench controller`.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::fmt::Write as _;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use ferry_controller::translate::{Settings, translate};
use ferry_controller::world::{ObjKey, World};

/// One Gateway and `routes` HTTPRoutes, each to its own Service with ten
/// ready endpoints.
fn world(routes: usize) -> World {
    let mut w = World::from_manifests(
        "kind: GatewayClass\nmetadata: { name: ferry }\nspec: { controllerName: gateway.ferry.dev/controller }\n---\n\
         kind: Gateway\nmetadata: { name: gw, namespace: apps }\nspec:\n  gatewayClassName: ferry\n  \
         listeners: [{ name: http, port: 80, protocol: HTTP }]\n",
    )
    .unwrap();
    // Parsed per route: the YAML loader caps one input at 1024 documents.
    for i in 0..routes {
        let mut y = String::new();
        let eps: Vec<String> = (0..10)
            .map(|e| format!("10.{}.{}.{e}", i / 250, i % 250))
            .collect();
        write!(
            y,
            "---\nkind: HTTPRoute\nmetadata: {{ name: r{i}, namespace: apps }}\nspec:\n  parentRefs: [{{ name: gw }}]\n  \
             hostnames: [svc{i}.example.com]\n  rules:\n    - matches: [{{ path: {{ type: PathPrefix, value: /api/{i} }}, headers: [{{ name: x-v, value: '2' }}] }}]\n      \
             backendRefs: [{{ name: s{i}, port: 80 }}]\n---\n\
             kind: Service\nmetadata: {{ name: s{i}, namespace: apps }}\nspec: {{ ports: [{{ port: 80, targetPort: 8080 }}] }}\n---\n\
             kind: EndpointSlice\nmetadata: {{ name: s{i}-x, namespace: apps, labels: {{ kubernetes.io/service-name: s{i} }} }}\n\
             addressType: IPv4\nports: [{{ port: 8080 }}]\nendpoints: [{}]\n",
            eps.iter().map(|a| format!("{{ addresses: [{a}] }}")).collect::<Vec<_>>().join(", ")
        )
        .unwrap();
        let part = World::from_manifests(&y).unwrap();
        w.http_routes.extend(part.http_routes);
        w.services.extend(part.services);
        w.endpoint_slices.extend(part.endpoint_slices);
    }
    w
}

fn controller(c: &mut Criterion) {
    let settings = Settings {
        controller_name: "gateway.ferry.dev/controller".into(),
        proxy_protocol: false,
    };
    for n in [100, 1000] {
        let w = world(n);
        c.bench_function(&format!("controller/translate_{n}_routes"), |b| {
            b.iter(|| black_box(translate(black_box(&w), &settings)))
        });
        let t = translate(&w, &settings);
        let config = &t.gateways[&ObjKey::new("apps", "gw")].config;
        assert_eq!(config.listeners[0].routes.len(), n);
        c.bench_function(&format!("controller/snapshot_json_{n}_routes"), |b| {
            b.iter(|| black_box(serde_json::to_vec(black_box(config)).unwrap()))
        });
    }
}

criterion_group!(benches, controller);
criterion_main!(benches);
