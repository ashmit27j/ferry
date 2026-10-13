# Benchmarks

## Micro-benchmarks (criterion)

```sh
cargo bench -p ferry-bench                    # all
cargo bench -p ferry-bench -- router/         # filter by name
```

Benches live in `benches/hot_path.rs`; fixtures in `src/lib.rs`. Criterion keeps
history under `target/criterion/`, so a second run reports change against the
first.

## Load tests (k6)

```sh
# terminal 1-3: backends
cargo run --release -p ferry-testkit --bin mock-backend -- --name a --http 127.0.0.1:9001
# terminal 4: proxy
cargo run --release -p ferry -- proxy --config examples/hello.yaml
# terminal 5: load (native k6)
k6 run -e TARGET=http://127.0.0.1:8080 -e RATE=10000 -e DURATION=60s bench/k6/proxy.js
# or with Docker (Linux: --network host and TARGET=127.0.0.1)
docker run --rm -i -e TARGET=http://host.docker.internal:8080 -e RATE=2000 grafana/k6:1.3.0 run - < bench/k6/proxy.js
```

**Measuring added latency.** Run the same scenario against a backend directly
and then through Ferry, and subtract the percentiles. The script uses a
constant-arrival-rate executor so slow responses do not reduce offered load
(no coordinated omission). The thresholds fail the run on any HTTP error or
dropped iteration.

Record every gate run in [`RESULTS.md`](RESULTS.md) with the environment.

## Milestone exit tests (Linux, in Docker)

`bench/exit/run-m1.sh` runs the M1 exit test entirely inside one Linux network
namespace, so Docker Desktop's port forwarding is not in the path: three mock
backends (cores 4-9), Ferry pinned to one core (core 0), a reloader that rewrites
the config every second, and k6 on cores 10-19 sharing the container's network.

```sh
# 1. Build Linux release binaries into a Docker volume (not into ./target).
MSYS_NO_PATHCONV=1 docker run --rm -v "$PWD:/src" -v ferry-target:/target \
  -v ferry-cargo:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/target -w /src \
  rust:1.99.0-bookworm cargo build --release --locked -p ferry -p ferry-testkit --bins
# 2. Run: baseline (k6 -> backend), steady (k6 -> Ferry), chaos (reloads + kill).
bench/exit/run-m1.sh
```

k6 summaries land in `bench/exit/results/`. `bench/exit/sockets.sh` counts
upstream and downstream sockets by TCP state inside the container, which is how
connection churn is checked. Extra `ferry proxy` flags go in `FERRY_ARGS`, e.g.
`FERRY_ARGS="--access-log /run/ferry/access.log" bench/exit/run-m1.sh` to
measure the cost of access logging.

`bench/exit/run-m3.sh` is the M3 exit run: Ferry, three traced mocks, an
OpenTelemetry Collector, Tempo, Prometheus and Grafana on one Docker network,
2,000 req/s from k6 for three minutes with a backend killed and reloads every
5 s, then traced requests. `bench/exit/m3-check.py` evaluates every dashboard
panel's query against Prometheus and follows each trace in Tempo; results go
to `bench/exit/results/m3-*.json`. See
[`deploy/observability`](../deploy/observability/README.md).

`bench/exit/run-m4.sh` drives a kind cluster from a runner container (kind,
kubectl, helm, the compiled Gateway API conformance suite): `FRESH=1`
recreates the cluster, and `PHASES` picks from `setup smoke chaos
conformance`. `chaos` (M5) scales the fleet to three proxies and keeps 500
req/s flowing from outside the cluster while it deletes proxy, leader
controller and backend pods, then force-kills one proxy in a separate
window.

## M5: soak, chaos and 10,000 WebSockets

`bench/exit/run-m5.sh` runs Ferry (pinned with `cpuset`), three HTTP
backends and a WebSocket backend in separate containers on a fixed subnet,
with Prometheus scraping Ferry every 5 s. `ws-flood` holds `WS_COUNT`
WebSockets for the whole run, sending a message on each every 30 s. k6 runs
in 5-minute segments, because it keeps every sample in memory. Every 20
minutes, `m5/chaos.sh` kills and restarts a backend, adds 200 ms of netem
latency to another, and then 5% packet loss to a third. `m5/m5-check.py`
then checks the SLOs and fits a line through resident memory and open file
descriptors after warm-up.

```sh
# The gate soak, on a dedicated Linux host (Docker; no WSL2 in the path):
DURATION=4h RATE=10000 FERRY_CPUSET=0 WS_COUNT=10000 bench/exit/run-m5.sh
# A 72-hour run is the same with DURATION=72h. A dry run:
DURATION=10m RATE=2000 WS_COUNT=2000 bench/exit/run-m5.sh
```

`bench/exit/m5/fuzz.sh` fuzzes every target in a nightly container (any
host), and `bench/exit/m5/alerts-test.sh` runs promtool on the alerting
rules.
