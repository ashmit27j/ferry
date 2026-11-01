# Ferry, free version

**This is the free version of Ferry: a public demo on Render's free tier.**
It runs the same `ferry` binary as a production install, but it is not a
production deployment. Use it to see Ferry route real traffic. Don't put
anything you care about behind it.

- URL: `https://ferry-free.onrender.com` (Render may add a suffix if the name
  is taken; the Render dashboard shows the real one)
- Config: [`deploy/render/ferry.yaml`](../deploy/render/ferry.yaml)
- Image: [`deploy/render/Dockerfile`](../deploy/render/Dockerfile)
- Blueprint: [`render.yaml`](../render.yaml)
- Why it is built this way: [ADR 0038](adr/0038-free-public-demo.md)

Open the URL in a browser: the landing page runs every demo below from
buttons, through Ferry, beside a live dashboard. The dashboard reads
`/stats` (Ferry's Prometheus metrics, rewritten to the admin API's
`/metrics`) every 2 seconds, so it shows everyone's traffic since the
container last woke. The first request after 15 idle minutes takes 30 to 60 seconds: the free
tier stops the container when it's idle and starts it on demand.

## Try it

Every demo response carries `x-ferry-edition: free`, and `x-mock-backend`
names the backend that answered.

```sh
URL=https://ferry-free.onrender.com

curl -i $URL/hello                      # round robin: a, b, a, b...
for i in $(seq 20); do curl -s $URL/canary -o /dev/null -D - | grep x-mock-backend; done
                                        # 90/10 canary split: 18 a, 2 b
curl -i -H 'x-canary: always' $URL/canary   # header match: always b
for i in $(seq 20); do curl -s -o /dev/null -w '%{http_code}\n' $URL/retry; done
                                        # backend fails 50%; Ferry retries 503s
curl -i $URL/nope                       # 404 from Ferry: no route
```

WebSocket echo: connect to `wss://ferry-free.onrender.com/ws` with any
client (for example `websocat`, or `cargo run -p ferry-testkit --bin ws-flood
-- --url wss://.../ws --count 5 --hold 10s`).

Requests are rate-limited to 50 per second (burst 100) across everyone, so
you may see 429s.

`deploy/render/smoke.sh [URL]` runs all of the above (and checks the admin
API is not public). Run it after a push to `main` redeploys the demo.

## How it differs from the real deal

The real deal is what the project is built for: Ferry on Kubernetes, behind
a cloud load balancer, configured by the Gateway API.

| | Free version (this demo) | The real deal (production install) |
| --- | --- | --- |
| Where it runs | One container on Render's free tier (0.1 CPU, 512 MB) | A proxy fleet per Gateway on Kubernetes (EKS or any cluster), 2+ replicas with an HPA and a PodDisruptionBudget |
| Configuration | A fixed YAML file baked into the image | The Gateway API (`Gateway`, `HTTPRoute`, `GRPCRoute`) and `FerryPolicy`, read by `ferry controller` and streamed to proxies over mTLS; no restarts |
| Kubernetes controller | Not running | Watches the cluster, writes route status, provisions proxies |
| Backends | Three mock echo servers inside the same container | Your Services and pods, discovered from EndpointSlices |
| TLS | Render terminates TLS in front of Ferry; Ferry sees plain HTTP, so backends get `x-forwarded-proto: http` | Ferry terminates TLS itself (SNI, hot certificate rotation, mTLS to backends) |
| Load balancer in front | Render's shared proxy | A cloud NLB with PROXY protocol v2, so Ferry sees real client IPs |
| HTTP/2 and gRPC from clients | Whatever Render's edge supports; gRPC is not demoed | Full HTTP/2 and gRPC, including gRPC deadlines and trailers |
| Availability | One instance, sleeps after 15 idle minutes, no SLO | Multiple replicas, zero-drop rolling restarts, SLOs with alerts |
| Observability | JSON logs in the Render dashboard; metrics are public at `/stats` and drawn live by the landing page; the rest of the admin API stays private | Prometheus metrics, Grafana dashboard, alerting rules with runbooks, OpenTelemetry traces |
| Image | Debian slim plus a shell to start the mocks, amd64 only | Distroless, no shell, read-only root, multi-arch, Cosign-signed with an SBOM |
| Rate limits | One global 50 req/s limit to protect a tiny instance | Per-route and per-key limits you set, split across replicas |
| Cost | Free | Whatever your cluster and load balancer cost |

Everything in the right-hand column is built and tested (see the README and
`bench/RESULTS.md`): conformance and pod-kill chaos run on a local kind
cluster, which is also free. What has not been done is a long-running
deployment on a cloud cluster.

## Run the free version yourself

Locally, without Docker:

```sh
cargo run -p ferry-testkit --bin mock-backend -- --name site --http 127.0.0.1:9004 --page deploy/render/index.html
cargo run -p ferry-testkit --bin mock-backend -- --name a --http 127.0.0.1:9001 --ws 127.0.0.1:9201
cargo run -p ferry-testkit --bin mock-backend -- --name b --http 127.0.0.1:9002
cargo run -p ferry-testkit --bin mock-backend -- --name flaky --http 127.0.0.1:9003 --error-rate 0.5
cargo run -p ferry -- proxy --config deploy/render/ferry.yaml   # listens on 0.0.0.0:10000
```

With Docker: `docker build -f deploy/render/Dockerfile -t ferry-free .` then
`docker run --rm -p 10000:10000 ferry-free`.

Your own Render copy: fork the repository, then in Render choose
**New > Blueprint** and pick the fork. `render.yaml` sets up the free
service, and every push to `main` redeploys it.
