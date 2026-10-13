# syntax=docker/dockerfile:1.7
# Ferry: one image for `ferry proxy`, `ferry controller` and `ferry certgen`.
#
#   docker build -t ferry:dev .
#
# The runtime image is distroless (glibc, CA roots, no shell) and runs as the
# nonroot user (65532); the Helm chart adds a read-only root filesystem.

FROM rust:1.99.0-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p ferry \
    && objcopy --strip-debug target/release/ferry /ferry

FROM gcr.io/distroless/cc-debian12:nonroot
LABEL org.opencontainers.image.source="https://github.com/ashmit27j/ferry" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.description="Ferry: a Kubernetes-native L7 load balancer and Gateway API implementation"
COPY --from=build /ferry /usr/local/bin/ferry
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/ferry"]
