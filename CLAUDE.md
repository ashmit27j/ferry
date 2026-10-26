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
FUZZ_SECONDS=600 bench/exit/m5/fuzz.sh        # every fuzz target in a nightly container (any host)
bench/exit/run-m1.sh                          # M1 exit test in Docker (see bench/README.md)
bench/exit/run-m3.sh                          # M3 exit: Prometheus/Grafana/Tempo stack, SLO + trace checks
FRESH=1 bench/exit/run-m4.sh                  # M4 exit: new kind cluster + MetalLB, helm install, smoke, conformance
EXTENDED=1 PHASES=conformance bench/exit/run-m4.sh   # conformance with the extended features claimed
DURATION=4h bench/exit/run-m5.sh              # M5 exit: soak + chaos + 10k WebSockets, SLO and memory checks
bench/exit/m5/alerts-test.sh                  # promtool check + unit tests of the alerting rules
cargo bench -p ferry-bench --bench controller # translation and snapshot serialisation
python crates/ferry-controller/gen/gateway_api.py v1.6.3 --crds   # regenerate Gateway API types + chart CRDs
cargo run -p ferry -- crds > deploy/helm/ferry/crds/ferrypolicies.yaml  # after changing FerryPolicy (a test checks)
docker build -t ferry:dev .                   # distroless image: proxy, controller, certgen
python deploy/grafana/generate.py deploy/grafana/ferry-dashboard.json   # then: cargo nextest run --test dashboard

cargo run -p ferry -- proxy --config examples/hello.yaml       # reloads the file on change
cargo run -p ferry -- check-config examples/gateway.yaml       # routing and balancing
cargo run -p ferry -- check-config examples/edge.yaml          # TLS, limits, resilience
cargo run -p ferry -- check-config --output json --print examples/hello.yaml
cargo run -p ferry -- proxy --controller https://ctl:18000 --gateway ns/gw --controller-tls-dir dir   # K8s mode
cargo run -p ferry -- controller --no-leader-election --proxy-template t.yaml   # needs a kubeconfig
cargo run -p ferry-testkit --bin mock-backend -- --name a --http 127.0.0.1:9001 --grpc 127.0.0.1:9101 --ws 127.0.0.1:9201
cargo run -p ferry-testkit --bin trace-client -- --url http://127.0.0.1:8080/ --otlp-endpoint http://127.0.0.1:4317
cargo run -p ferry-testkit --bin ws-flood -- --url ws://127.0.0.1:8080/ws --count 1000 --hold 60s
curl localhost:9901/metrics                   # admin API: /live /ready /metrics /config /clusters
```

All of fmt, clippy, nextest, deny and audit must pass before every commit. CI
(`.github/workflows/ci.yml`) runs exactly these, plus coverage. The nightly
workflow fuzzes each target for 10 minutes. `release.yml` runs release-please
on main; merging its release PR publishes a multi-arch image and the Helm
chart to GHCR, Cosign-signed (keyless), with an SPDX SBOM and provenance.
Every action is pinned by commit SHA (version in a comment); keep it so.

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
Kubernetes (ferry-controller, every replica):
  reflectors (GatewayClass, Gateway, HTTPRoute, GRPCRoute, ReferenceGrant, Service,
  EndpointSlice, TLS Secret, Namespace, FerryPolicy, own Deployments) ─▶ notify
  ─▶ debounce 200 ms (or 30 s resync) ─▶ World ─▶ translate() (pure) ─▶ Translation
     ├─ per Gateway: Config (rate limits ÷ ready replicas) ─▶ Hub::publish (if changed)
     │     ─▶ SnapshotService stream (mTLS) ─▶ proxy: parse+validate, swap, ACK | NACK
     └─ leader only (Lease): status patches (only if changed) + fleet SSA
        (Deployment, Service, PDB, HPA per Gateway, in the controller namespace)
YAML ──ferry-config──▶ Config ──into_snapshot()──▶ Snapshot (validated, immutable, versioned)
  ▲ watch.rs polls the file                         │
  │              prepare() loads certs/CAs (fallible, no side effects), then
  │              Control::build reuses unchanged clusters/routers/limiters,
  │              endpoint state, pools, health checkers ─▶ Runtime ─ArcSwap::store
accept (closed if the listener holds max_connections) ─▶ [PROXY v1/v2] ─▶ [TLS: SNI cert, ALPN h2/http1.1] ─▶ hyper auto (h1/h2, upgrades)
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
       ─▶ 101: byte tunnel (tunnel.rs: tunnel_idle timeout, growing buffers, counted
          as an open connection); else response filters, idle-timed body
Exchange (observe.rs): span + log fields at arrival ▶ route/cluster/reason/upstream
       wait filled in ▶ at headers: overhead histogram ▶ Observed body: on finish or
       drop, RED counters + duration, span end, access-log line (writer thread)
admin (axum, own port): /live /ready (503 draining) /metrics /config /clusters
       /metrics = Metrics families + StateCollector (reads Runtime at scrape,
       including certificate expiry); ferry_controller_connected once following
health checkers (per endpoint, TLS-aware) ─▶ EndpointState ─▶ ClusterBalancer::refresh
maintenance loop: outlier sweep 500 ms, cert re-read 1 s, idle pool sweep 15 s
shutdown: drain.delay (accept, Connection: close) ▶ GOAWAY/close ▶ drain.timeout
reload: plan listener slots (keep / rename / bind new / retire) ▶ prepare ▶ build ▶
       swap ▶ retire old accept loops, start new ones (ADR 0036)
```

- `ferry-config` (API frozen, ADR 0013; M2 and M4 added only optional fields;
  new fields must skip serialising their default, ADR 0033):
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
- `ferry-proxy`: `server.rs` (bind/`bind_with(Telemetry)`/`bind_awaiting`, listener
  slots bound and retired on reload, accept, PROXY header,
  TLS handshake, drain stages, maintenance loop, `ReloadHandle`), `state.rs` (`Runtime`,
  `ListenerRuntime`, `ClusterRuntime`, `prepare`, `Control`, ADR 0014),
  `router.rs` (ADR 0009), `forward.rs` (filters, edge headers, retries, breakers,
  outlier recording, upgrades, local replies), `grpc.rs` (deadlines,
  trailers-only replies, ADR 0018), `body.rs` (size limit, idle timers),
  `ratelimit.rs` (GCRA slots, ADR 0022), `tls.rs` (server SNI resolver and
  rotation, client configs, ADR 0023/0024), `proxy_protocol.rs` (ADR 0017),
  `watch.rs` (ADR 0015), `observe.rs` (`Exchange`, `Observed` body),
  `collector.rs` (scrape-time gauges), `admin.rs` (ADR 0030), `remote.rs`
  (follows the controller's snapshot stream, ADR 0033), `tunnel.rs` (relay
  with idle timeout, ADR 0037).
- Metric handles live in the runtime (`ListenerRuntime.stats`/`route_stats`/
  `unmatched`, `ClusterRuntime.stats`); never look a metric up per request.
  New metrics: add to the stats struct and the `Families` collector, then to
  the dashboard; `tests/it/dashboard.rs` checks every queried name exists.
- Local replies: 404 no route, 500 zero weights or invalid backendRefs, 503 no healthy upstream,
  503 + `x-ferry-overloaded` (breaker), 429 + `retry-after` (rate limit),
  502 upstream failure, 504 timeout, 413/431/408/400 client errors; gRPC
  requests get trailers-only responses with the mapped `grpc-status`.
- Each request calls `load_full()` once and keeps that `Arc<Runtime>`, so
  in-flight requests finish on their original snapshot.
- `ferry-api`: `ferry.v1.SnapshotService` (bidirectional stream, hello names the
  Gateway, ACK/NACK by nonce). Codegen via protox (ADR 0003).
- `ferry-controller` (ADR 0032-0035): `gateway_api/` (kopium-generated from the
  v1.6.3 CRDs by `gen/gateway_api.py`; never edit by hand), `policy.rs`
  (`FerryPolicy` CRD, `gateway.ferry.dev/v1alpha1`, rate-limit split),
  `world.rs` (`World`, `ObjKey`, `Fleet`; `World::from_manifests` for tests),
  `translate/{mod,route,backend,hostname,tests}.rs` (the pure translator:
  listeners, attachment, backends, conditions), `status.rs` (status JSON,
  write only on change), `provision.rs` (`ProxyTemplate`, fleet rendering),
  `server.rs` (`Hub`, gRPC server), `leader.rs` (Lease), `certgen.rs`,
  `run.rs` (reflectors, reconcile loop, writes, `/healthz` `/readyz`
  `/metrics` `/debug/proxies` on 18001), `metrics.rs` (controller metrics,
  read from the `Hub` at scrape).
- Gateway API semantics live in `translate`; test them there with YAML
  fixtures (`translate/tests.rs`), not on a cluster. Container ports are
  Gateway ports below 1024 plus 10000; 19001 is the proxy admin port.
- `deploy/helm/ferry`: chart (CRDs, certgen hook, controller, RBAC,
  GatewayClass, optional Gateway, PodMonitors, PrometheusRule from
  `files/alerts.yaml`, NetworkPolicies); `values-kind.yaml` for kind. The
  proxy template ConfigMap must match `ProxyTemplate` (unknown fields are
  rejected); the controller renders each fleet's NetworkPolicy.
- Alerts: every rule in `files/alerts.yaml` needs a runbook in
  `docs/runbooks/` (listed in its README) and only queries exported metrics
  (`tests/it/alerts.rs`); behaviour is unit-tested with promtool
  (`bench/exit/m5/alerts-test.yaml`). Proxy series carry `fleet` (from the
  PodMonitor), so group alerts by it.
- Security docs: `SECURITY.md`, `docs/security/threat-model.md` (update it
  when adding a listener, parser, permission or trust boundary).
- `bench/exit/m5/`: soak config, `ferry-env.sh`, `chaos.sh` (kill, netem
  latency and loss), `m5-check.py` (SLOs and memory slope from
  Prometheus), `fuzz.sh`, promtool tests.
- `bench/exit/m4/`: runner image (kind, kubectl, helm, docker CLI, compiled
  conformance suite), `e2e.sh` (phases setup, smoke, conformance), `smoke.yaml`.
- `ferry-testkit`: mocks with `Behavior { latency, error_rate, error_status,
  tracing }` (`with_tracing` makes the HTTP mock record server spans);
  `trace-client` and `ws-flood` (hold N WebSockets) binaries;
  the HTTP mock honours `x-mock-latency-ms`, `x-mock-status` and
  `x-mock-body-delay-ms`, and `http::spawn_tls` reports ALPN and client
  certificates. `MockServer::set_healthy` flips `/healthz` and gRPC health.
- `tests/`: `TcpBalancer` (L4 stand-in for an NLB), `run_load`, raw-frame
  HTTP/2 attacks, gRPC interop, TLS/mTLS, drain and rolling restart;
  `TestProxy::start_with_telemetry`, `serve_admin`, `metrics_text`, `metric`;
  `tracing.rs` runs an OTLP collector in-process; `dashboard.rs`, `alerts.rs`.
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
- Every parser of untrusted input (requests, headers, configs, snapshots,
  manifests) gets a fuzz target in `fuzz/` with seeds, and joins the nightly
  matrix. Values Ferry prints must parse back (fuzz targets check this).
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
  editing with Python on Windows, open files with `newline='\n'`. Link
  errors about PDBs (LNK1201/LNK1318) or "os error 112" mean the disk is
  full: `cargo clean --profile dev` (dev builds keep line tables only).

## Milestone status

| Milestone | Status |
| --- | --- |
| M0 Foundations | Approved 2026-10-13 |
| M1 Core proxy | Approved 2026-10-13 |
| M2 TLS and resilience | Approved 2026-10-13 |
| M3 Observability and admin | Approved 2026-10-13 |
| M4 Kubernetes controller | Approved 2026-10-13 |
| **M5 Hardening** | Complete except the long soak (deferred to a Linux host), awaiting gate approval |
| M6 Production beta | Not started |

### Deferred out of M5 (the plan owner's call)

- **The long soak** runs on a Linux host, not the WSL2 dev box: `DURATION=4h`
  (or 72h) `RATE=10000 FERRY_CPUSET=0 bench/exit/run-m5.sh`. A 30-minute
  partial run passed every SLO (bench/RESULTS.md, M5); flat memory still
  needs the long run. The M6 staging run (48 h) and production week cover
  the 72-hour requirement.
- **p99 latency** end to end is still unproven off the WSL2 VM. Ferry's own
  histogram reads p50 13 µs / p99 25 µs at 10k req/s on one core.

### Known gaps (current)

- Snapshot-stream certificates are read at start: after certgen renews them
  (`helm upgrade` within 90 days of expiry), restart controller and proxies
  (threat model R4). All proxies share one client identity, so a
  compromised proxy can fetch any Gateway's snapshot (R1, planned for M6).
- The controller can read every Secret (RBAC cannot select by type; R2).
- No Gateway listener isolation (`GatewayHTTPListenerIsolation`), ListenerSet,
  BackendTLSPolicy, RequestMirror, CORS, GEP-1731 HTTPRoute retries (use
  FerryPolicy `retry`), static Gateway addresses, `parametersRef` or
  per-Gateway infrastructure overrides.
- Conformance and pod-kill chaos run on kind with MetalLB; EKS with the AWS
  Load Balancer Controller and PROXY v2 is untested until M6 staging.
- Rate-limit splitting and connection caps assume an even spread of clients
  across replicas.
- CL+TE requests are decoded by Transfer-Encoding and the connection closed,
  not answered 400 (hyper does not expose the conflict; ADR 0016).
- Upstream client certificates and CAs rotate on config change, not by
  polling files (listener certificates do poll; ADR 0024).
- WebSockets over HTTP/2 (RFC 8441) and upgrades to h2 clusters are
  unsupported (ADR 0025). Clusters that carry many WebSockets need circuit
  breakers above their peak tunnel count (ADR 0037).
- Upstream `max_connections` fails fast instead of queueing (ADR 0020).
- Ferry's diagnostic logs carry no trace ID (access-log lines do). OTLP
  export is plaintext gRPC only.
- `edge/proxy_protocol_v1_parse` benchmarks slower than in M2 with no code
  change; unexplained, still < 0.1 µs per connection.
- Dependabot PRs bump the SHA-pinned actions to new majors.
