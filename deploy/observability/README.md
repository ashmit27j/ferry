# Local observability stack

Configs for running Ferry's telemetry end to end on one machine, in Docker:

| Container | Image | Role |
| --- | --- | --- |
| `m3-otel` | `otel/opentelemetry-collector` | Receives OTLP/gRPC from Ferry and the backends, forwards to Tempo |
| `m3-tempo` | `grafana/tempo` | Stores traces; API on `localhost:3200` |
| `m3-prom` | `prom/prometheus` | Scrapes Ferry's admin API every 5 s; `localhost:9090` |
| `m3-grafana` | `grafana/grafana` | Provisioned with Prometheus, Tempo and the Ferry dashboard; `localhost:3000` |
| `ferry-m3` | `rust` + built binaries | Ferry (admin on `:9901`, tracing, access log) and three traced mock backends |

`bench/exit/run-m3.sh` starts all of it, drives load with k6, kills a backend,
sends traced requests with `trace-client`, and checks the result with
`bench/exit/m3-check.py`. It leaves the stack running (`KEEP=0` tears it
down), so afterwards open <http://localhost:3000> (anonymous admin) and the
**Ferry** dashboard, or look up a trace ID from
`bench/exit/results/m3-trace-ids.txt` in Grafana's Explore view (Tempo).

Build the Linux binaries first; see [`bench/README.md`](../../bench/README.md).

In Kubernetes the same pieces exist already (plan §6): kube-prometheus-stack
scrapes the admin port, and Ferry exports to the in-cluster OpenTelemetry
Collector. Import `deploy/grafana/ferry-dashboard.json` into Grafana there.
