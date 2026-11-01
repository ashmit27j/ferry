# syntax=docker/dockerfile:1.7
# Ferry, free version: the public demo image for Render (ADR 0038).
#
#   docker build -f deploy/render/Dockerfile -t ferry-free .
#   docker run --rm -p 10000:10000 ferry-free
#
# Unlike the production image (../../Dockerfile), this one bundles the mock
# backends and a shell to start them, and builds for amd64 only.

FROM rust:1.99.0-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p ferry -p ferry-testkit --bin ferry --bin mock-backend \
    && strip target/release/ferry target/release/mock-backend \
    && cp target/release/ferry target/release/mock-backend /usr/local/bin/

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/ferry /usr/local/bin/mock-backend /usr/local/bin/
COPY deploy/render/ferry.yaml /etc/ferry/ferry.yaml
COPY --chmod=0755 deploy/render/start.sh /usr/local/bin/start.sh
USER 65532:65532
EXPOSE 10000
ENTRYPOINT ["/usr/local/bin/start.sh"]
