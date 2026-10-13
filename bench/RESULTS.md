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

## M2 — 2026-10-13 (regression check, same Linux VM as M1)

Same environment and script as M1 (`bench/exit/run-m1.sh`), with the M2 build:
TLS, limits, timeouts, retries, rate limits, breakers and outlier detection
compiled in, and `X-Forwarded-*` and `X-Request-Id` added to every request.

| Run | Requests at 10k/s | Failed | p50 | p90 | p99 |
| --- | --- | --- | --- | --- | --- |
| Backend direct | 300,001 | 0 | 0.088 ms | 0.231 ms | 0.84 ms |
| Through Ferry, steady | 299,825 | 0 | 0.163 ms | 0.211 ms | 5.2 ms |
| **Through Ferry, kill + 66 reloads** | **599,966** | **0** | 0.162 ms | 0.211 ms | 2.4 ms |

- The M1 exit criterion still holds: zero errors under kill and reload.
- p50 rose about 11 µs over M1 (0.152 → 0.163 ms): the new per-request work
  (edge headers, a generated request ID, limit and breaker checks).
- k6 dropped 174 and 35 iterations in the two Ferry runs (0.06% and 0.006%),
  a generator-side artefact as in M1; no request failed.
- The p99 analysis from M1 applies unchanged (loopback wakeups in the VM);
  per the plan owner, latency is re-measured on the Linux deployment target.

### Micro-benchmarks (`cargo bench -p ferry-bench`, Windows, i7-12700F)

| Benchmark | M1 | M2 |
| --- | --- | --- |
| `lb/round_robin_pick_16` | 23.9 ns | 38.0 ns |
| `lb/weighted_round_robin_pick_16` | 37.8 ns | 56.0 ns |
| `lb/least_request_pick_16` | 20.7 ns | 20.8 ns |
| `lb/p2c_ewma_pick_16` | 71.7 ns | 71.8 ns |
| `lb/maglev_pick_16` | 16.9 ns | 16.8 ns |
| `lb/backend_split_pick` | 21.2 ns | 29.3 ns |
| `lb/retry_budget_deposit` (new) | — | 35.7 ns |
| `lb/retry_budget_withdraw` (new) | — | 35.3 ns |
| `lb/breaker_acquire_release` (new) | — | 15.8 ns |
| `edge/proxy_protocol_v1_parse` (new) | — | 22.5 ns |
| `edge/proxy_protocol_v2_parse` (new) | — | 6.1 ns |
| `router/100_routes_prefix_hit` | 43.7 ns | 45.2 ns |
| `router/1000_routes_prefix_hit` | 44.5 ns | 43.9 ns |
| `config/parse_validate_100_routes` | 2.44 ms | 2.44 ms |

The round-robin, weighted and backend-split picks read 30–50% slower than in
M1, but `lb/backend_split_pick` exercises `WeightedSelector`, whose code did not
change in M2. Treat those three as run-to-run noise on a desktop machine (no
CPU pinning, turbo enabled), not a regression; the gate on a dedicated Linux
host will settle it.

## M3 — 2026-10-13 (exit run and regression check, same Linux VM as M1)

### Exit criteria (`bench/exit/run-m3.sh`)

Ferry with metrics, tracing (1% of new traces, plus every traced caller) and
the access log on; three traced mock backends; OpenTelemetry Collector 0.130,
Tempo 2.8.1, Prometheus 3.5.0 (5 s scrapes of the admin API) and Grafana
12.1.0, all in Docker. k6 sent 2,000 req/s for 180 s; backend `c` was killed
at t = 60 s and the config reloaded every 5 s. This run checks observability,
not latency (nothing is CPU-pinned).

**Load:** 360,000 requests, **0 failed, 0 dropped iterations**; p50 0.26 ms,
p99 0.42 ms (k6, through Docker networking).

**Every SLO read from the dashboard.** `bench/exit/m3-check.py` ran each SLO
panel's query through Prometheus (variables at "all", `$__rate_interval` =
1m, `$__range` = 5m) at the end of the run (`results/m3-check.json`):

| SLO panel | Value | SLO |
| --- | --- | --- |
| Added latency p50 | 13 µs | < 0.2 ms |
| Added latency p99 | 45 µs (p99.9 60 µs) | < 1 ms |
| Requests per core-second | 7,266 | context for "per core"¹ |
| Success ratio | 100% | ≥ 99.95% |
| Failed or cut-off requests over the range | 0 | 0 during reloads (12/min) |
| Resident memory growth | −7 MB/h (31 MB RSS) | flat |

25 of 27 panels returned data; "Local replies by reason" (the 429s happened
in the first seconds, outside the last minute's rate) and "gRPC status" (no
gRPC traffic) were empty, as expected. Grafana loaded the provisioned
dashboard (uid `ferry-overview`) and answered its queries through its
Prometheus datasource.

¹ At 2,000 req/s with access log and tracing on and an idle-ish event loop,
each request costs more CPU than at 10k req/s, where batching amortises
wakeups; this panel is for the 10k req/s gate on the deployment target.

**One trace spans client → Ferry → backend.** `trace-client` sent three
traced requests; Tempo returned each trace with four spans from three
services, chained:

```
trace-client:CLIENT GET -> ferry:SERVER "GET bench" -> ferry:CLIENT GET -> mock-b:SERVER "GET /traced"
```

The added-latency figures come from Ferry's own histogram (arrival to response
headers minus upstream waits). They exclude kernel and network time, so they
are not the same measurement as the k6 subtraction below; both are reported.

### Regression check: M1 exit test with M3 telemetry (`bench/exit/run-m1.sh`)

Same environment as M1 and M2: Ferry on one core, 10k req/s. Metrics are always
on and the admin API listens; tracing is off.

| Run | Requests | Failed | Dropped it. | p50 | p90 | p99 |
| --- | --- | --- | --- | --- | --- | --- |
| Backend direct | 299,874 | 0 | 127 | 0.086 ms | 0.165 ms | 0.342 ms |
| Ferry, steady (metrics) | 300,001 | 0 | 0 | 0.161 ms | 0.202 ms | 1.00 ms |
| **Ferry, kill + 66 reloads (metrics)** | **600,001** | **0** | **0** | 0.161 ms | 0.203 ms | 0.520 ms |
| Ferry, steady (metrics + access log) | 299,973 | 0 | 28 | 0.189 ms | 0.239 ms | 2.20 ms |
| Ferry, kill + reloads (metrics + access log) | 600,002 | 0 | 0 | 0.184 ms | 0.233 ms | 0.975 ms |

- Zero errors under kill and reload, as in M1 and M2.
- Metrics cost nothing visible at p50 (0.163 ms in M2, 0.161 ms now).
- The access log adds about 21–27 µs at p50 (serialising the line and handing
  it to the writer thread, plus the writer's own CPU on the same core).
- p99 moves between 0.5 and 5 ms from run to run in this VM, in both
  directions, for the loopback reasons analysed under M1. It is still unproven
  and is measured on the Linux deployment target.
- Ferry's RSS after the chaos run: 48 MB (M1: 42 MB).

### Micro-benchmarks (`cargo bench -p ferry-bench`, Windows, i7-12700F)

| Benchmark | M2 | M3 |
| --- | --- | --- |
| `telemetry/histogram_observe` (new) | — | 9.1 ns |
| `telemetry/route_record` (counter + histogram, new) | — | 13.0 ns |
| `telemetry/access_log_line` (serialise one line, new) | — | 647 ns |
| `telemetry/spans_unsampled` (server + client span, inject, new) | — | 763 ns |
| `telemetry/spans_sampled` (new) | — | 1.05 µs |
| `telemetry/scrape_100_routes` (encode /metrics, new) | — | 312 µs |
| `lb/round_robin_pick_16` | 38.0 ns | 23.8 ns |
| `lb/weighted_round_robin_pick_16` | 56.0 ns | 37.6 ns |
| `lb/backend_split_pick` | 29.3 ns | 21.3 ns |
| `lb/maglev_pick_16` | 16.8 ns | 16.9 ns |
| `lb/retry_budget_deposit` | 35.7 ns | 36.0 ns |
| `lb/breaker_acquire_release` | 15.8 ns | 15.9 ns |
| `edge/proxy_protocol_v1_parse` | 22.5 ns | 82.9 ns² |
| `edge/proxy_protocol_v2_parse` | 6.1 ns | 8.4 ns |
| `router/100_routes_prefix_hit` | 45.2 ns | 43.8 ns |
| `router/1000_routes_prefix_hit` | 43.9 ns | 44.3 ns |
| `config/parse_validate_100_routes` | 2.44 ms | 2.46 ms |

The round-robin, weighted and split picks are back at their M1 values, which
supports reading M2's numbers as noise.

² `proxy_protocol.rs` did not change in M3, yet v1 parsing reads 3.7× slower,
reproducibly across two runs today. It is still under 0.1 µs per connection
(not per request). Recorded as an open item to profile, not explained.

Per request, telemetry costs about 22 ns with tracing and the access log off,
about 0.7 µs more with the access log on, and about 0.8 µs more with tracing on.

## M4 — 2026-10-13 (exit run on kind, dev workstation)

### Exit criteria (`FRESH=1 bench/exit/run-m4.sh`)

kind v1.37.0 (one node) with MetalLB for LoadBalancer addresses, in Docker
Desktop. The run deleted any existing cluster, created a new one, loaded the
`ferry:dev` image and ran `helm install` with `values-kind.yaml` (Gateway
API v1.6.3 standard CRDs, two controller replicas).

**`helm install` on a fresh kind cluster yields a working gateway**
(`results/m4-run.log`, `m4-smoke.*`). The Helm-created Gateway was
`Programmed=True` with a MetalLB address 5 s after the release came up. An
HTTPRoute with a FerryPolicy served `GET /smoke/hello` through it to an echo
pod, with `X-Forwarded-For`, `X-Forwarded-Proto` and `X-Request-Id` set.
Route and policy status were written back:

```
ferry: Accepted=True ResolvedRefs=True
FerryPolicy Accepted=True (Accepted)
```

**The Gateway API conformance suite passes for HTTPRoute and GRPCRoute**
(v1.6.3 suite, profiles GATEWAY-HTTP and GATEWAY-GRPC):

| Run | Profile | Core | Extended | Sub-tests |
| --- | --- | --- | --- | --- |
| Core features (`m4-conformance-core.*`) | GATEWAY-HTTP | 36 passed, 0 failed | — | 215 passed, 0 failed |
| | GATEWAY-GRPC | 14 passed, 0 failed | — | |
| Extended features claimed (`m4-conformance-extended.*`) | GATEWAY-HTTP | 36 passed, 0 failed | **19 passed, 0 failed** | 331 passed, 0 failed |
| | GATEWAY-GRPC | 14 passed, 0 failed | — | |

The 16 extended features Ferry claims are listed in the extended report:
GatewayPort8080, the 303/307/308 redirect codes, port, scheme and path
redirects, host and path rewrites, query and method matching, response header
modification, request and backend timeouts, parentRef port, and the H2C and
WebSocket backend protocols. Not claimed: listener isolation, BackendTLSPolicy,
CORS, request mirroring, static addresses, infrastructure overrides (see
CLAUDE.md, "Known gaps carried out of M4").

### Control-plane benchmarks (`cargo bench -p ferry-bench --bench controller`, Windows, i7-12700F)

One Gateway; each HTTPRoute goes to its own Service with ten ready endpoints.
Every change in the cluster runs one translation (after a 200 ms debounce);
the snapshot JSON is what each proxy receives.

| Benchmark | Mean |
| --- | --- |
| `controller/translate_100_routes` | 0.81 ms |
| `controller/translate_1000_routes` | 8.49 ms |
| `controller/snapshot_json_100_routes` | 124 µs |
| `controller/snapshot_json_1000_routes` | 1.39 ms |

Translation scales linearly at about 8.5 µs per route. At 1,000 routes a
reconcile pass costs under 10 ms of CPU, well inside the debounce window.

### Micro-benchmarks (`cargo bench -p ferry-bench`, Windows, i7-12700F)

M4 did not touch the request path. The listener changes (ADR 0036) only run
on reload.

| Benchmark | M3 | M4 |
| --- | --- | --- |
| `lb/round_robin_pick_16` | 23.8 ns | 25.6 ns |
| `lb/weighted_round_robin_pick_16` | 37.6 ns | 39.7 ns |
| `lb/backend_split_pick` | 21.3 ns | 22.1 ns |
| `lb/maglev_pick_16` | 16.9 ns | 18.5 ns |
| `lb/retry_budget_deposit` | 36.0 ns | 37.8 ns |
| `lb/breaker_acquire_release` | 15.9 ns | 16.6 ns |
| `edge/proxy_protocol_v1_parse` | 82.9 ns | 91.3 ns |
| `edge/proxy_protocol_v2_parse` | 8.4 ns | 8.7 ns |
| `router/100_routes_prefix_hit` | 43.8 ns | 48.2 ns |
| `router/1000_routes_prefix_hit` | 44.3 ns | 48.9 ns |
| `telemetry/route_record` | 13.0 ns | 13.7 ns |
| `telemetry/access_log_line` | 647 ns | 671 ns |
| `telemetry/scrape_100_routes` | 312 µs | 332 µs |
| `config/parse_validate_100_routes` | 2.46 ms | 2.64 ms |

Everything is 3–10% slower. The benchmarks ran while the kind cluster and the
conformance suite were up in Docker on the same machine, so this is read as
load from the shared box, not a regression. The M1 exit test was not
re-run for M4.
