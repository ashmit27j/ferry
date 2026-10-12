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
