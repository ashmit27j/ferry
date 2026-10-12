# Benchmark results

Each milestone gate appends a section. Numbers are only comparable within one
environment; the gate environment for SLO claims (M1 onwards) is a dedicated
Linux host or EKS node, recorded with each run.

SLO targets (plan §5): added latency p50 < 0.2 ms and p99 < 1 ms at 10k req/s
per core; zero errors with healthy backends, during reloads and during rolling
restarts.

## M0 — 2026-10-13 (baseline, dev workstation)

**Environment:** Windows 11, Intel i7-12700F (12 cores), 32 GB RAM, Rust 1.99.0,
release profile (thin LTO, codegen-units = 1). The system allocator is used
because jemalloc is not built on MSVC.

### Micro-benchmarks (`cargo bench -p ferry-bench`)

| Benchmark | Median |
| --- | --- |
| `lb/round_robin_pick` | 11.4 ns |
| `lb/weighted_pick` (3 backends, 90/5/5) | 23.1 ns |
| `router/100_routes_prefix_hit` | 360 ns |
| `router/100_routes_mixed_case_host_with_port` | 368 ns |
| `router/100_routes_miss` | 219 ns |
| `config/parse_validate_100_routes` | 2.37 ms |

Routing is a linear scan over every route on the listener. At 100 routes it is
already about a third of a microsecond. M1 adds a hostname index (exact map plus
a wildcard suffix trie) before the 10k req/s gate.

### k6 smoke (`bench/k6/proxy.js`, constant arrival rate)

One `mock-backend` (0 ms latency) and `ferry proxy --config examples/hello.yaml`
ran on the host, with k6 1.3.0 in Docker Desktop reaching them through
`host.docker.internal`. Each run lasted 20 s at 2,000 req/s.

| Target | Requests | Failed | p50 | p90 | p99 | p99.9 |
| --- | --- | --- | --- | --- | --- | --- |
| Backend direct | 39,996 | 0 | 0.448 ms | 0.641 ms | 1.17 ms | 10.7 ms |
| Through Ferry | 40,001 | 0 | 0.516 ms | 0.734 ms | 1.38 ms | 8.63 ms |
| **Added by Ferry** | | | **≈ 0.07 ms** | ≈ 0.09 ms | ≈ 0.21 ms | n/a (noise) |

**Caveats.** The Docker Desktop VM network path dominates the tail; p99.9 is
noise in both runs. The direct run dropped 5 iterations while k6 warmed up its
virtual users. This is a smoke test that the harness works, not an SLO
measurement. The M1 gate repeats it at 10k req/s across 3 backends on Linux,
with the load generator pinned to separate cores.
