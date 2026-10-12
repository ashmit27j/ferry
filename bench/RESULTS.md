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

## M1 — 2026-10-13 (exit test, Linux VM)

**Environment:** Docker Desktop 4.x on Windows 11, WSL2 kernel
6.18.33.2-microsoft-standard, 20 vCPUs on an Intel i7-12700F, 16 GB. Linux
release binaries (Rust 1.99.0, thin LTO, jemalloc). Ferry was pinned to one core
(`taskset -c 0`, so tokio ran one worker); mocks on cores 4-9; k6 1.3.0 on cores
10-19; all on loopback inside one network namespace. Script:
`bench/exit/run-m1.sh`; k6 summaries in `bench/exit/results/`.

### Exit criterion: 10k req/s across 3 backends, zero errors, while (a) a backend is killed and (b) config reloads every second

| Run | Requests | Rate | Failed | Dropped iterations | p50 | p90 | p99 | p99.9 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Backend direct (baseline) | 300,000 | 10,000/s | 0 | 0 | 0.088 ms | 0.236 ms | 0.853 ms | 9.72 ms |
| Through Ferry, steady | 300,001 | 10,000/s | 0 | 0 | 0.152 ms | 0.198 ms | 4.11 ms | 24.3 ms |
| **Through Ferry, chaos** | **600,001** | **10,000/s** | **0** | **0** | 0.152 ms | 0.200 ms | 3.43 ms | 28.3 ms |

Chaos run: 66 config reloads in 66 s (each one a real change: a response header
value), and `kill -9` of backend `c` at t = 20 s. Ferry logged `endpoint
unhealthy` for `c` about 2 s later (interval 1 s × threshold 2) and rebuilt the
host set to 2 endpoints. Requests sent to `c` in the meantime were retried on
another endpoint (ADR 0011), so none failed. Ferry's RSS was 42 MB after the run.

**Met:** zero errors under kill and reload at 10,000 req/s on one core.

### SLO check (plan §5): added latency at 10k req/s per core

- **p50:** 0.152 − 0.088 ≈ **0.06 ms** (target < 0.2 ms): met.
- **p99:** 4.11 − 0.85 ≈ **3.3 ms** (target < 1 ms): **not met in this
  environment.** Run-to-run spread was 4–8 ms at p99 (four more steady runs with
  Ferry on cores 0, 2, 3 and 2-3).

What the measurements say about the p99 gap:

- Ferry used **~43% of its core** at 10k req/s (`top`), so the tail is not CPU
  saturation, and giving Ferry two cores did not remove it (p99 4.35 ms).
- Upstream connections were stable (558 established, no TIME_WAIT growth during
  the run), so it is not pool churn.
- `perf` on the Ferry process: **59% of samples in the kernel, 33% in one call
  chain**: `writev` → loopback transmit → the *receiver's* TCP stack runs inline
  → `sock_def_readable` → wake the peer process → `_raw_spin_unlock_irqrestore`.
  On loopback in a VM, every write Ferry makes pays for waking k6 or a mock on
  another vCPU (an IPI and a hypervisor exit). A proxied request makes four such
  wakeups against two for the direct baseline. With a real NIC that cost is not
  on the sender's core.
- An earlier run with fewer pre-allocated k6 VUs showed p99 1.28 ms, but it
  dropped 2% of iterations, which hides exactly the slow requests (coordinated
  omission), so it is not counted.

**Next:** repeat on a dedicated Linux host or EKS node with load generator and
backends on other machines before drawing conclusions about Ferry's own tail.
Candidate Ferry-side costs, should the tail persist there: the per-request
`HeaderMap` copy kept for safe replay, and the per-request task that returns an
HTTP/1.1 connection to the pool.

### Micro-benchmarks (`cargo bench -p ferry-bench`, Windows, i7-12700F)

| Benchmark | M0 | M1 |
| --- | --- | --- |
| `lb/backend_split_pick` (rule backends 90/5/5) | 23.1 ns | 21.2 ns |
| `lb/round_robin_pick_16` | 11.4 ns¹ | 23.9 ns |
| `lb/weighted_round_robin_pick_16` | — | 37.8 ns |
| `lb/least_request_pick_16` | — | 20.7 ns |
| `lb/p2c_ewma_pick_16` | — | 71.7 ns |
| `lb/maglev_pick_16` | — | 16.9 ns |
| `lb/maglev_rebuild_16` (65,537 slots) | — | 946 µs |
| `router/100_routes_prefix_hit` | 360 ns | **43.7 ns** |
| `router/100_routes_mixed_case_host_with_port` | 368 ns | 62.3 ns |
| `router/100_routes_miss` | 219 ns | 20.5 ns |
| `router/1000_routes_prefix_hit` | — | 44.5 ns |
| `router/1000_routes_mixed_case_host_with_port` | — | 68.1 ns |
| `config/parse_validate_100_routes` | 2.37 ms | 2.44 ms |

¹ M0's round robin ignored health. M1's health-aware pick loads the current host
set through `ArcSwap` (about 10 ns) before indexing it.

The router is now flat in the number of routes: a hostname index (ADR 0009)
replaced M0's linear scan.
