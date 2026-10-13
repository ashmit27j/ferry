//! Micro-benchmarks for per-request work: balancer picks, routing and config load.
//!
//! Run with `cargo bench -p ferry-bench`. Record results in `bench/RESULTS.md`.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use ferry_bench::route_table_yaml;
use ferry_config::{SnapshotVersion, from_yaml_str};
use ferry_lb::maglev::hash_key;
use ferry_lb::{
    Balancer, ClusterBalancer, EndpointState, Host, Limit, Policy, RetryBudget, WeightedSelector,
};
use ferry_proxy::Router;
use ferry_proxy::router::RequestInfo;
use http::{HeaderMap, Method};

fn hosts(n: usize) -> Vec<Host> {
    (0..n)
        .map(|i| Host {
            state: Arc::new(EndpointState::new(
                format!("10.0.{}.{}:8080", i / 250, i % 250 + 1)
                    .parse()
                    .unwrap(),
            )),
            weight: u32::try_from(i % 4 + 1).unwrap(),
        })
        .collect()
}

fn resilience(c: &mut Criterion) {
    let budget = RetryBudget::new(20, 10, Duration::from_secs(10));
    c.bench_function("lb/retry_budget_deposit", |b| b.iter(|| budget.deposit()));
    c.bench_function("lb/retry_budget_withdraw", |b| {
        b.iter(|| black_box(budget.try_withdraw()))
    });
    let limit = Limit::new(1024);
    c.bench_function("lb/breaker_acquire_release", |b| {
        b.iter(|| drop(black_box(limit.try_acquire())))
    });
    let mut v2 = b"\r\n\r\n\0\r\nQUIT\n".to_vec();
    v2.extend_from_slice(&[
        0x21, 0x11, 0x00, 0x0c, 192, 0, 2, 1, 10, 0, 0, 1, 0xc7, 0x38, 0x01, 0xbb,
    ]);
    c.bench_function("edge/proxy_protocol_v2_parse", |b| {
        b.iter(|| black_box(ferry_proxy::proxy_protocol::parse(black_box(&v2))))
    });
    let v1 = b"PROXY TCP4 192.0.2.1 10.0.0.1 51000 443\r\n";
    c.bench_function("edge/proxy_protocol_v1_parse", |b| {
        b.iter(|| black_box(ferry_proxy::proxy_protocol::parse(black_box(v1))))
    });
}

fn balancers(c: &mut Criterion) {
    let weighted = WeightedSelector::new(&[90, 5, 5]);
    c.bench_function("lb/backend_split_pick", |b| {
        b.iter(|| black_box(weighted.pick()))
    });
    for (name, policy) in [
        ("round_robin", Policy::RoundRobin),
        ("weighted_round_robin", Policy::WeightedRoundRobin),
        ("least_request", Policy::LeastRequest),
        ("p2c_ewma", Policy::P2cEwma),
        ("maglev", Policy::Maglev),
    ] {
        let lb = ClusterBalancer::new(hosts(16), policy, 50, 0);
        let key = hash_key(b"user-1234");
        c.bench_function(&format!("lb/{name}_pick_16"), |b| {
            b.iter(|| black_box(lb.pick(black_box(Some(key)), &[])))
        });
    }
    c.bench_function("lb/maglev_rebuild_16", |b| {
        let lb = ClusterBalancer::new(hosts(16), Policy::Maglev, 50, 0);
        b.iter(|| lb.refresh());
    });
}

fn routing(c: &mut Criterion) {
    let empty = HeaderMap::new();
    for n in [100, 1000] {
        let snap = from_yaml_str(&route_table_yaml(n))
            .unwrap()
            .into_snapshot(SnapshotVersion(1))
            .unwrap();
        let router = Router::new(&snap.listeners()[0]);
        let host = format!("svc{}.example.com", n / 2);
        let upper = format!("SVC{}.Example.com:443", n / 2);
        let route = |host: &str, path: &str| {
            router
                .route(&RequestInfo {
                    host: black_box(host),
                    method: &Method::GET,
                    path: black_box(path),
                    query: None,
                    headers: &empty,
                })
                .is_some()
        };
        c.bench_function(&format!("router/{n}_routes_prefix_hit"), |b| {
            b.iter(|| black_box(route(&host, "/api/v1/users")))
        });
        c.bench_function(
            &format!("router/{n}_routes_mixed_case_host_with_port"),
            |b| b.iter(|| black_box(route(&upper, "/healthz"))),
        );
        c.bench_function(&format!("router/{n}_routes_miss"), |b| {
            b.iter(|| black_box(route("nope.example.org", "/")))
        });
    }
}

fn config(c: &mut Criterion) {
    let yaml = route_table_yaml(100);
    c.bench_function("config/parse_validate_100_routes", |b| {
        b.iter(|| {
            from_yaml_str(black_box(&yaml))
                .unwrap()
                .into_snapshot(SnapshotVersion(1))
                .unwrap()
        })
    });
}

criterion_group!(benches, balancers, resilience, routing, config);
criterion_main!(benches);
