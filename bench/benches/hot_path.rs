//! Micro-benchmarks for per-request work: balancer picks, routing and config load.
//!
//! Run with `cargo bench -p ferry-bench`. Record results in `bench/RESULTS.md`.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use ferry_bench::route_table_yaml;
use ferry_config::{SnapshotVersion, from_yaml_str};
use ferry_lb::{Balancer, RoundRobin, WeightedSelector};
use ferry_proxy::Router;

fn balancers(c: &mut Criterion) {
    let rr = RoundRobin::new(16);
    c.bench_function("lb/round_robin_pick", |b| b.iter(|| black_box(rr.pick())));
    let weighted = WeightedSelector::new(&[90, 5, 5]);
    c.bench_function("lb/weighted_pick", |b| {
        b.iter(|| black_box(weighted.pick()))
    });
}

fn routing(c: &mut Criterion) {
    let snap = from_yaml_str(&route_table_yaml(100))
        .unwrap()
        .into_snapshot(SnapshotVersion(1))
        .unwrap();
    let router = Router::new(&snap.listeners()[0]);
    c.bench_function("router/100_routes_prefix_hit", |b| {
        b.iter(|| {
            black_box(router.route(black_box("svc57.example.com"), black_box("/api/v1/users")))
        })
    });
    c.bench_function("router/100_routes_mixed_case_host_with_port", |b| {
        b.iter(|| {
            black_box(router.route(black_box("SVC57.Example.com:443"), black_box("/healthz")))
        })
    });
    c.bench_function("router/100_routes_miss", |b| {
        b.iter(|| black_box(router.route(black_box("nope.example.org"), black_box("/"))))
    });
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

criterion_group!(benches, balancers, routing, config);
criterion_main!(benches);
