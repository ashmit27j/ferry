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

cargo run -p ferry -- proxy --config examples/hello.yaml
cargo run -p ferry -- check-config examples/hello.yaml
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
                                                     │
                                ferry-proxy::Runtime (routers + balancer state)
                                                     │  ArcSwap::store on reload
accept ─▶ hyper auto (h1 / h2c) ─▶ Router (host+path precedence) ─▶ WeightedSelector (backendRefs)
       ─▶ RoundRobin (endpoints) ─▶ pooled hyper client (HTTP/1.1 upstream) ─▶ backend
```

- `ferry-config`: `model.rs` (serde types, `deny_unknown_fields`), `validate.rs`
  (collects every error with a field path), `snapshot.rs` (private fields,
  accessors, match predicates), `load.rs` (4 MiB cap, serde-saphyr).
- `ferry-lb`: the `Balancer` trait, `RoundRobin` and `WeightedSelector` (ADR 0007).
  Atomics only.
- `ferry-proxy`: `server.rs` (bind, accept loop, `GracefulShutdown` drain,
  `ReloadHandle::swap`), `router.rs` (Gateway API precedence, ADR 0005),
  `forward.rs` (hop-by-hop stripping, Host preservation, local replies:
  404 no route, 500 all-zero weights, 502 upstream failure), `state.rs`.
- Each request calls `load_full()` once and keeps that `Arc<Runtime>`, so
  in-flight requests finish on their original snapshot.
- `ferry-api`: `ferry.v1.SnapshotService` (bidirectional stream, ACK/NACK by
  nonce). Codegen via protox (ADR 0003).
- `ferry-testkit`: mocks with `Behavior { latency, error_rate, error_status }`;
  the HTTP mock also honours `x-mock-latency-ms` and `x-mock-status` per request.

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
  Ask the human only for irreversible or security-relevant choices.
- Hot path: no locks, no blocking calls, avoid per-request allocation where
  practical; use `ArcSwap`, atomics and pre-computed state in `Runtime`.
- Windows dev boxes work (jemalloc is disabled on MSVC; SIGTERM handling is
  Unix-only). CI on Linux is authoritative.

## Milestone status

| Milestone | Status |
| --- | --- |
| **M0 Foundations** | Complete, awaiting gate approval |
| M1 Core proxy | Not started |
| M2 TLS and resilience | Not started |
| M3 Observability and admin | Not started |
| M4 Kubernetes controller | Not started |
| M5 Hardening | Not started |
| M6 Production beta | Not started |

### Known gaps carried out of M0

- Upstream is HTTP/1.1 only; h2/gRPC upstream arrives in M2.
- Routing scans routes linearly (≈ 360 ns at 100 routes); M1 adds a host index.
- Balancer counters reset on reload; M1 must carry state for stateful balancers.
- No file watching yet; hot reload from file lands in M1 (`ReloadHandle::swap` exists).
- No X-Forwarded-*, request IDs, timeouts, retries or limits yet (M2/M3).
- `ferry controller` exits with "not implemented (M4)".
- GitHub Actions are pinned by major tag, not commit SHA; pin SHAs before the repo
  goes public (M5).
