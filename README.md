# Ferry

A Kubernetes-native Layer 7 load balancer and API gateway, written in Rust.

Ferry terminates TLS and proxies HTTP/1.1, HTTP/2, gRPC and WebSockets to backend
pods. It is configured through the Kubernetes Gateway API and runs on AWS EKS
behind a Network Load Balancer.

> **Status: M0 (foundations).** The workspace, CI, mock backends and a minimal
> HTTP/1.1 + h2c proxy exist. TLS, the controller and the full feature set are
> planned for M1–M5. See [`CLAUDE.md`](CLAUDE.md) for milestone status.

```mermaid
%%{init: {"theme": "base", "themeVariables": {"fontFamily": "Inter, Helvetica Neue, Arial", "primaryColor": "#ffffff", "primaryTextColor": "#171717", "primaryBorderColor": "#dfdfdf", "lineColor": "#707070", "secondaryColor": "#fafafa", "tertiaryColor": "#ffffff"}}}%%
flowchart LR
    client([Client]) --> nlb[AWS NLB<br/>PROXY v2]
    nlb --> proxy
    subgraph cluster [EKS]
      controller[ferry controller<br/>Gateway API reconciler] -- snapshots over gRPC --> proxy[ferry proxy]
      proxy --> a[Pods: service A]
      proxy --> b[Pods: service B]
    end
    classDef ferry stroke:#3ecf8e,stroke-width:2px;
    class proxy,controller ferry;
```

## Quick start

```sh
# 1. A mock backend
cargo run -p ferry-testkit --bin mock-backend -- --name hello --http 127.0.0.1:9001

# 2. The proxy
cargo run -p ferry -- proxy --config examples/hello.yaml

# 3. A request
curl -s http://127.0.0.1:8080/hello
# {"backend":"hello","method":"GET","path":"/hello",...}
```

Validate a config without running it, or print its JSON Schema:

```sh
cargo run -p ferry -- check-config examples/hello.yaml
cargo run -p ferry -- check-config --schema > ferry.schema.json
```

## Repository layout

| Path | What |
| --- | --- |
| `bin/ferry` | Single binary: `proxy`, `controller`, `check-config` |
| `crates/ferry-config` | Config model, validation, immutable snapshots |
| `crates/ferry-lb` | Balancers (health checks, outlier detection and pools from M1) |
| `crates/ferry-proxy` | Listeners, router, forwarding, hot swap, drain |
| `crates/ferry-controller` | Gateway API reconciler (M4) |
| `crates/ferry-api` | Protobuf + gRPC for controller → proxy snapshots |
| `crates/ferry-telemetry` | Logging (metrics and tracing in M3) |
| `crates/ferry-testkit` | Mock HTTP/h2c/gRPC/WebSocket backends |
| `tests/` | In-process integration tests |
| `fuzz/` | cargo-fuzz targets |
| `bench/` | criterion, k6, [results](bench/RESULTS.md) |
| `docs/adr/` | Architecture decision records |

## Development

See [`CLAUDE.md`](CLAUDE.md) for commands and conventions, and
[`DESIGN.md`](DESIGN.md) for the visual design system used by the dashboard and docs.

## License

[Apache License 2.0](LICENSE).
