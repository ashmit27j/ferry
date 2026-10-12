# CLAUDE.md

Guidance for working in this repository. Keep it current: update it at the end of
every milestone.

## What Ferry is

A Kubernetes-native L7 load balancer and API gateway in Rust: TLS termination,
HTTP/1.1, HTTP/2, gRPC and WebSocket proxying, configured by the Gateway API,
running on EKS behind an NLB. The full plan, milestones and SLOs live in the
master prompt; decisions live in `docs/adr/`.

## Commands

```sh
cargo build --workspace
cargo nextest run --workspace                 # all tests (unit, property, integration, e2e)
cargo test --doc --workspace                  # doctests (nextest does not run them)
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all
cargo deny --all-features check
cargo audit --deny warnings
cargo llvm-cov nextest --workspace --summary-only
cargo bench -p ferry-bench                    # criterion; record in bench/RESULTS.md
cargo +nightly fuzz run config_yaml fuzz/corpus/config_yaml fuzz/seeds/config_yaml   # nightly + cargo-fuzz, Linux/macOS
bench/exit/run-m1.sh                          # M1 exit test in Docker (see bench/README.md)

cargo run -p ferry -- proxy --config examples/hello.yaml       # reloads the file on change
cargo run -p ferry -- check-config examples/gateway.yaml       # every M1 feature
cargo run -p ferry-testkit --bin mock-backend -- --name a --http 127.0.0.1:9001 --grpc 127.0.0.1:9101 --ws 127.0.0.1:9201
```

All of fmt, clippy, nextest, deny and audit must pass before every commit. CI
(`.github/workflows/ci.yml`) runs exactly these, plus coverage. The nightly
workflow fuzzes each target for 10 minutes.

Logging: `FERRY_LOG` takes an `EnvFilter` (default `info`); `--log-format json`
or `FERRY_LOG_FORMAT=json` switches to JSON lines on stderr.

## Architecture (as built)

```
YAML ──ferry-config──▶ Config ──into_snapshot()──▶ Snapshot (validated, immutable, versioned)
  ▲ watch.rs polls the file                         │
  │                         Control::build (reuses unchanged clusters/routers,
  │                         endpoint state, pools, health checkers) ─▶ Runtime
  │                                                  │  ArcSwap::store
accept ─▶ hyper auto (h1 / h2c) ─▶ Router (host index ▶ path/method/header/query)
       ─▶ redirect | request filters ─▶ WeightedSelector (backendRefs)
       ─▶ ClusterBalancer (health-aware host set; RR/WRR/LR/P2C-EWMA/Maglev)
       ─▶ Pool (h1 idle queue | shared h2c) ─▶ backend
          └─ retry on another endpoint if the request provably never arrived
health checkers (per endpoint) ─▶ EndpointState.healthy ─▶ ClusterBalancer::refresh
```

- `ferry-config` (API frozen, ADR 0013): `model.rs` (serde types, all
  `#[non_exhaustive]` with constructors), `duration.rs` (GEP-2257),
  `validate/{mod,routes,clusters}.rs` (collects every error with a field path),
  `snapshot/{mod,matching,filter,cluster}.rs` (private fields, accessors, match
  predicates, redirect/rewrite logic), `load.rs` (4 MiB cap, serde-saphyr).
- `ferry-lb`: `cluster.rs` (`ClusterBalancer`, `HostSet`, panic mode, five
  policies, ADR 0010), `endpoint.rs` (`EndpointState`: health, in-flight, peak
  EWMA), `maglev.rs`, `health.rs` (HTTP and hand-rolled gRPC probes, ADR 0012),
  `pool.rs` (own HTTP/1.1 and h2c pools, ADR 0011), `weighted.rs` (ADR 0007).
- `ferry-proxy`: `server.rs` (bind, accept, drain, `ReloadHandle`, control lock),
  `state.rs` (`Runtime`, `Control` reuse across reloads, ADR 0014; hash keys and
  stickiness cookies), `router.rs` (hostname index, ADR 0009), `forward.rs`
  (filters, safe retries, local replies: 404 no route, 500 zero weights,
  503 no healthy upstream, 502 upstream failure), `body.rs`, `watch.rs`
  (ADR 0015).
- Each request calls `load_full()` once and keeps that `Arc<Runtime>`, so
  in-flight requests finish on their original snapshot.
- `ferry-api`: `ferry.v1.SnapshotService` (bidirectional stream, ACK/NACK by
  nonce). Codegen via protox (ADR 0003).
- `ferry-testkit`: mocks with `Behavior { latency, error_rate, error_status }`;
  the HTTP mock also honours `x-mock-latency-ms` and `x-mock-status` per request.
  `MockServer::set_healthy` flips `GET /healthz` (HTTP) and
  `grpc.health.v1.Health/Check` (gRPC).

## Conventions

- **No `unwrap`/`expect`/`panic!`/`todo!` in library code**: enforced by workspace
  lints (ADR 0006). Tests are exempt, and integration test crates carry a
  crate-level `#![allow]`. No `unsafe` (`forbid`); if it seems needed, write an
  ADR first.
- Typed errors with `thiserror` in libraries; `anyhow` only in binaries.
- Every public item is documented (`missing_docs` is an error in CI). Every module
  has unit tests. Every bug fix comes with a regression test.
- Use proptest for balancers, matching, retry budgets and config validation.
- Conventional commits (`feat(lb): ...`, `fix(proxy): ...`, `docs(adr): ...`),
  small and focused.
- Ambiguity: do what Envoy or the Gateway API does, write an ADR, and continue.
  Ask the human only for irreversible or security-relevant choices. That
  includes widening the `deny.toml` license allow-list: prefer an alternative
  crate (xxhash-rust, BSL-1.0, was swapped for twox-hash, MIT).
- `ferry-config` is frozen (ADR 0013): only additive changes without a new ADR.
- Hot path: no locks, no blocking calls, avoid per-request allocation where
  practical; use `ArcSwap`, atomics and pre-computed state in `Runtime`. Anything
  that allocates (host sets, Maglev tables) is rebuilt off the hot path.
- Windows dev boxes work (jemalloc is disabled on MSVC; SIGTERM handling is
  Unix-only; a refused TCP connect takes ~2 s, so retry tests are slow there).
  CI on Linux is authoritative. Files must stay LF (`.gitattributes`); when
  editing with Python on Windows, open files with `newline='\n'`.

## Milestone status

| Milestone | Status |
| --- | --- |
| M0 Foundations | Approved 2026-10-13 |
| **M1 Core proxy** | Complete, awaiting gate approval |
| M2 TLS and resilience | Not started |
| M3 Observability and admin | Not started |
| M4 Kubernetes controller | Not started |
| M5 Hardening | Not started |
| M6 Production beta | Not started |

### Known gaps carried out of M1

- **p99 added latency is unproven.** At 10k req/s on one core in a WSL2 VM,
  p50 added ≈ 0.06 ms (meets the SLO) but p99 added ≈ 3–7 ms; `perf` attributes a
  third of Ferry's CPU to loopback wakeups of peer processes in the VM
  (bench/RESULTS.md). Re-measure on a dedicated Linux host or EKS node.
- CI has never run: the repo has no remote yet.
- No downstream or upstream TLS, PROXY protocol, X-Forwarded-*, request IDs,
  timeouts, configurable retries, circuit breakers, outlier ejection, rate limits
  or security limits (M2). WebSocket upgrades are not proxied yet (M2).
- No metrics, tracing, access logs or admin API (M3).
- Listener changes (add, remove, re-address) need a restart; a removed listener
  answers 503.
- Health probes open a new connection each time.
- `ferry controller` exits with "not implemented (M4)".
- GitHub Actions are pinned by major tag, not commit SHA; pin SHAs before the repo
  goes public (M5).
