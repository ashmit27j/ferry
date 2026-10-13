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
bench/exit/run-m3.sh                          # M3 exit: Prometheus/Grafana/Tempo stack, SLO + trace checks
python deploy/grafana/generate.py deploy/grafana/ferry-dashboard.json   # then: cargo nextest run --test dashboard

cargo run -p ferry -- proxy --config examples/hello.yaml       # reloads the file on change
cargo run -p ferry -- check-config examples/gateway.yaml       # routing and balancing
cargo run -p ferry -- check-config examples/edge.yaml          # TLS, limits, resilience
cargo run -p ferry -- check-config --output json --print examples/hello.yaml
cargo run -p ferry-testkit --bin mock-backend -- --name a --http 127.0.0.1:9001 --grpc 127.0.0.1:9101 --ws 127.0.0.1:9201
cargo run -p ferry-testkit --bin trace-client -- --url http://127.0.0.1:8080/ --otlp-endpoint http://127.0.0.1:4317
curl localhost:9901/metrics                   # admin API: /live /ready /metrics /config /clusters
```

All of fmt, clippy, nextest, deny and audit must pass before every commit. CI
(`.github/workflows/ci.yml`) runs exactly these, plus coverage. The nightly
workflow fuzzes each target for 10 minutes.

Logging: `FERRY_LOG` takes an `EnvFilter` (default `info`); `--log-format json`
or `FERRY_LOG_FORMAT=json` switches to JSON lines on stderr.

`ferry proxy` observability flags: `--admin-address` (default `127.0.0.1:9901`,
`off`; `FERRY_ADMIN_ADDRESS`), `--access-log stdout|stderr|<path>|off`
(default off; `FERRY_ACCESS_LOG`), `--otlp-endpoint` (tracing off unless set;
`OTEL_EXPORTER_OTLP_ENDPOINT`), `--service-name` (`OTEL_SERVICE_NAME`),
`--trace-sample-ratio` (default 1.0, parent-based). Tests that spawn the
binary pass `--admin-address off` (or a free port) so they do not collide.

## Architecture (as built)

```
YAML ──ferry-config──▶ Config ──into_snapshot()──▶ Snapshot (validated, immutable, versioned)
  ▲ watch.rs polls the file                         │
  │              prepare() loads certs/CAs (fallible, no side effects), then
  │              Control::build reuses unchanged clusters/routers/limiters,
  │              endpoint state, pools, health checkers ─▶ Runtime ─ArcSwap::store
accept ─▶ [PROXY v1/v2] ─▶ [TLS: SNI cert, ALPN h2/http1.1] ─▶ hyper auto (h1/h2, upgrades)
       ─▶ limits (headers, body 413, h2 streams, Rapid Reset caps)
       ─▶ Router (host index ▶ path/method/header/query) ─▶ route rate limit (GCRA, 429)
       ─▶ redirect | request filters + X-Forwarded-For/Proto + X-Request-Id
       ─▶ WeightedSelector (backendRefs) ─▶ deadline (route timeout ∧ grpc-timeout)
       ─▶ attempts: breakers (requests/pending/connections/retries) ─▶ ClusterBalancer
          (health ∧ not ejected; RR/WRR/LR/P2C-EWMA/Maglev) ─▶ Pool (h1 idle queue |
          shared h2; plain or TLS/mTLS) ─▶ backend
          ├─ safe retry: request never arrived (connect fail, unsent, idempotent reset)
          ├─ policy retry: codes / reset / per-try timeout, spends the retry budget
          └─ outcome ▶ outlier detection (eject, sweep back)
       ─▶ 101: byte tunnel (WebSocket); else response filters, idle-timed body
Exchange (observe.rs): span + log fields at arrival ▶ route/cluster/reason/upstream
       wait filled in ▶ at headers: overhead histogram ▶ Observed body: on finish or
       drop, RED counters + duration, span end, access-log line (writer thread)
admin (axum, own port): /live /ready (503 draining) /metrics /config /clusters
       /metrics = Metrics families + StateCollector (reads Runtime at scrape)
health checkers (per endpoint, TLS-aware) ─▶ EndpointState ─▶ ClusterBalancer::refresh
maintenance loop: outlier sweep 500 ms, cert re-read 1 s, idle pool sweep 15 s
shutdown: drain.delay (accept, Connection: close) ▶ GOAWAY/close ▶ drain.timeout
```

- `ferry-config` (API frozen, ADR 0013; M2 added only optional fields):
  `model.rs` and `policy.rs` (serde types, all `#[non_exhaustive]` with
  constructors), `duration.rs` (GEP-2257), `validate/{mod,routes,clusters,policy}.rs`
  (collects every error with a field path), `snapshot/{mod,matching,filter,
  cluster,policy}.rs` (private fields, accessors; `Secret` redacts keys),
  `load.rs` (4 MiB cap, serde-saphyr).
- `ferry-lb`: `cluster.rs` (`ClusterBalancer`, host sets, panic mode, five
  policies, ADR 0010; outlier hooks), `endpoint.rs` (`EndpointState`: health,
  in-flight, peak EWMA, outlier state), `outlier.rs` (ADR 0021), `budget.rs`
  (retry budget, ADR 0019), `breaker.rs` (`Limit`/`Permit`, ADR 0020),
  `maglev.rs`, `health.rs` (HTTP and gRPC probes, TLS-aware, ADR 0012),
  `pool.rs` (HTTP/1.1 and h2 pools, plain or TLS, ADR 0011/0024), `weighted.rs`.
- `ferry-telemetry` (ADR 0027–0029): `metrics.rs` (`Metrics` registry of
  `ListenerStats`/`RouteStats`/`ClusterStats` by name, `retain` on reload,
  `Reason` for local replies, `encode::Family` helper for collectors),
  `histogram.rs` (lock-free fixed buckets), `process.rs` (procfs), `trace.rs`
  (`Tracing`: server/client spans, W3C extract/inject, OTLP/gRPC batch export),
  `access_log.rs` (`AccessLog` writer thread, `Entry`), `lib.rs` (`Telemetry`
  bundle, `init_logging`).
- `ferry-proxy`: `server.rs` (bind/`bind_with(Telemetry)`, accept, PROXY header,
  TLS handshake, drain stages, maintenance loop, `ReloadHandle`), `state.rs` (`Runtime`,
  `ListenerRuntime`, `ClusterRuntime`, `prepare`, `Control`, ADR 0014),
  `router.rs` (ADR 0009), `forward.rs` (filters, edge headers, retries, breakers,
  outlier recording, upgrades, local replies), `grpc.rs` (deadlines,
  trailers-only replies, ADR 0018), `body.rs` (size limit, idle timers),
  `ratelimit.rs` (GCRA slots, ADR 0022), `tls.rs` (server SNI resolver and
  rotation, client configs, ADR 0023/0024), `proxy_protocol.rs` (ADR 0017),
  `watch.rs` (ADR 0015), `observe.rs` (`Exchange`, `Observed` body),
  `collector.rs` (scrape-time gauges), `admin.rs` (ADR 0030).
- Metric handles live in the runtime (`ListenerRuntime.stats`/`route_stats`/
  `unmatched`, `ClusterRuntime.stats`); never look a metric up per request.
  New metrics: add to the stats struct and the `Families` collector, then to
  the dashboard; `tests/it/dashboard.rs` checks every queried name exists.
- Local replies: 404 no route, 500 zero weights, 503 no healthy upstream,
  503 + `x-ferry-overloaded` (breaker), 429 + `retry-after` (rate limit),
  502 upstream failure, 504 timeout, 413/431/408/400 client errors; gRPC
  requests get trailers-only responses with the mapped `grpc-status`.
- Each request calls `load_full()` once and keeps that `Arc<Runtime>`, so
  in-flight requests finish on their original snapshot.
- `ferry-api`: `ferry.v1.SnapshotService` (bidirectional stream, ACK/NACK by
  nonce). Codegen via protox (ADR 0003).
- `ferry-testkit`: mocks with `Behavior { latency, error_rate, error_status,
  tracing }` (`with_tracing` makes the HTTP mock record server spans);
  `trace-client` binary;
  the HTTP mock honours `x-mock-latency-ms`, `x-mock-status` and
  `x-mock-body-delay-ms`, and `http::spawn_tls` reports ALPN and client
  certificates. `MockServer::set_healthy` flips `/healthz` and gRPC health.
- `tests/`: `TcpBalancer` (L4 stand-in for an NLB), `run_load`, raw-frame
  HTTP/2 attacks, gRPC interop, TLS/mTLS, drain and rolling restart;
  `TestProxy::start_with_telemetry`, `serve_admin`, `metrics_text`, `metric`;
  `tracing.rs` runs an OTLP collector in-process; `dashboard.rs`.
- `deploy/grafana/` (dashboard + generator, ADR 0031), `deploy/observability/`
  (collector, Tempo, Prometheus, Grafana provisioning for run-m3.sh).

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
| M1 Core proxy | Approved 2026-10-13 |
| M2 TLS and resilience | Approved 2026-10-13 |
| **M3 Observability and admin** | Complete, awaiting gate approval |
| M4 Kubernetes controller | Not started |
| M5 Hardening | Not started |
| M6 Production beta | Not started |

### Known gaps carried out of M3

- Added latency from Ferry's own histogram is p50 13 µs / p99 45 µs (exit
  run), but end-to-end p99 from k6 is still unproven off the WSL2 VM (see
  bench/RESULTS.md); measured on the Linux deployment target.
- No alerting rules or runbooks yet (M5); no certificate-expiry metric yet
  (needs X.509 parsing; M5 alert).
- Ferry's diagnostic logs carry no trace ID (access-log lines do). OTLP export
  is plaintext gRPC only.
- `edge/proxy_protocol_v1_parse` benchmarks 3.7× slower than in M2 with no
  code change; unexplained, still < 0.1 µs per connection.
- Docker Desktop is needed for run-m1.sh/run-m3.sh on Windows; the Chrome
  extension was not connected, so the gate has no dashboard screenshot.

### Known gaps carried out of M2

- **p99 latency is unproven** (WSL2 loopback artefacts; see bench/RESULTS.md).
  The plan owner deferred latency to the Linux deployment target.
- CL+TE requests are decoded by Transfer-Encoding and the connection closed,
  not answered 400 (hyper does not expose the conflict; ADR 0016).
- Upstream client certificates and CAs rotate on config change, not by polling
  files (listener certificates do poll; ADR 0024).
- WebSockets over HTTP/2 (RFC 8441) and upgrades to h2 clusters are unsupported
  (ADR 0025). Upgraded tunnels have no idle timeout.
- `max_connections` fails fast instead of queueing (ADR 0020).
- Rate limits are per replica; the controller's split arrives in M4.
- Listener changes (add, remove, re-address) need a restart.
- `ferry controller` exits with "not implemented (M4)".
- GitHub Actions are pinned by major tag, not commit SHA; pin SHAs before the repo
  goes public (M5). Dependabot PRs for action bumps are open.
