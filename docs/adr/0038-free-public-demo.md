# ADR 0038: A free public demo instead of a paid EKS beta

- **Status:** Accepted
- **Date:** 2026-11-01
- **Milestone:** M6

## Context

The plan's M6 was a production beta on EKS: a 48-hour staging run behind an
NLB, then a production week. That costs money, and on 2026-11-01 the plan
owner took paid hosting out of scope: Ferry should be publicly reachable for
free. The owner also waived M5's long soak, so M5 closes without it.

Free hosts don't run Kubernetes or give a raw TCP load balancer, so the
public deployment cannot be the production architecture. The options were:

- **Render free web service**: Docker from the repository, a public HTTPS
  URL, signup with GitHub only. One container, 0.1 CPU and 512 MB, stops
  after 15 idle minutes, TLS terminated by Render.
- **Koyeb free**: similar, also scales to zero.
- **Oracle Cloud Always Free VM**: a real VM that could run k3s, the Helm
  chart and the controller, with Ferry terminating TLS. Needs a card for
  signup and SSH-level operations.

## Decision

Deploy a clearly labelled **free version** on Render's free tier from a
Blueprint (`render.yaml`):

- One container (`deploy/render/Dockerfile`) runs `ferry proxy` with a fixed
  config (`deploy/render/ferry.yaml`) and three mock backends on loopback,
  started by `deploy/render/start.sh`. Ferry runs as PID 1 so Render's
  SIGTERM starts a normal drain.
- The config demos round robin, a 90/10 canary, a header match, retries
  against a backend that fails half the time, and WebSockets. `/` serves a
  landing page (`deploy/render/index.html`, from a fourth mock started with
  `--page`) that runs each demo from the browser and links to
  `docs/free-version.md`, which states the differences from a production
  install. Demo responses carry `x-ferry-edition: free`. (At first `/`
  redirected to the docs on GitHub; a visitor saw only the repository.)
- Only port 10000 is public. The admin API binds to loopback. One route,
  `/stats` (rate-limited to 20/s), rewrites to its `/metrics` so the
  landing page can draw a live dashboard; `/config`, `/clusters` and the
  rest stay private. The metrics carry route, cluster and listener names,
  counters and process stats, all of which the public config already
  reveals; no client addresses or secrets.
- A route rate limit (50/s, burst 100), a 1 MiB body cap and a 2000
  connection cap keep a shared 0.1 CPU instance usable.
- The repository is public so the docs link resolves and anyone can deploy
  their own copy from the Blueprint.

The production image, Helm chart and controller are unchanged. Kubernetes
remains tested for free on kind (`bench/exit/run-m4.sh`, `run-m5.sh`).

## Consequences

- Anyone can reach a Ferry instance; its config and code are public. The
  threat model gains boundary B7.
- The demo proves the proxy, not the platform: no controller, no Gateway
  API, no Ferry-terminated TLS, no PROXY v2, no SLO. `docs/free-version.md`
  says so.
- Render builds from source on every push to `main`. A broken build leaves
  the previous deploy running.
- EKS staging, the production week and p99 measured on dedicated hardware
  stay unproven. If paid hosting comes back, the original M6 plan still
  applies; Oracle's free VM is the closest free stand-in.
