# syntax=docker/dockerfile:1.7
# Ferry: one image for `ferry proxy`, `ferry controller` and `ferry certgen`.
#
#   docker build -t ferry:dev .
#   docker buildx build --platform linux/amd64,linux/arm64 -t ferry:multi .
#
# The build stage runs on the build host's platform and cross-compiles for
# the target, so an arm64 image does not compile Rust under QEMU. The
# runtime image is distroless (glibc, CA roots, no shell) and runs as the
# nonroot user (65532); the Helm chart adds a read-only root filesystem.

FROM --platform=$BUILDPLATFORM rust:1.99.0-bookworm AS build
ARG TARGETARCH
WORKDIR /src
RUN case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-gnu > /target-triple; arch=x86-64 ;; \
      arm64) echo aarch64-unknown-linux-gnu > /target-triple; arch=aarch64 ;; \
      *) echo "unsupported TARGETARCH $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && if [ "$(uname -m)" != "$( [ "$TARGETARCH" = arm64 ] && echo aarch64 || echo x86_64)" ]; then \
         apt-get update && apt-get install -y --no-install-recommends "gcc-${arch}-linux-gnu" "libc6-dev-${TARGETARCH}-cross" \
         && rm -rf /var/lib/apt/lists/*; \
       fi \
    && rustup target add "$(cat /target-triple)"
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=ferry-target-$TARGETARCH \
    triple=$(cat /target-triple) \
    && if [ "$TARGETARCH" = arm64 ] && [ "$(uname -m)" != aarch64 ]; then \
         export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
                CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
                AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar; \
         objcopy=aarch64-linux-gnu-objcopy; \
       elif [ "$TARGETARCH" = amd64 ] && [ "$(uname -m)" != x86_64 ]; then \
         export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
                CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
                AR_x86_64_unknown_linux_gnu=x86_64-linux-gnu-ar; \
         objcopy=x86_64-linux-gnu-objcopy; \
       else \
         objcopy=objcopy; \
       fi \
    && cargo build --release --locked -p ferry --target "$triple" \
    && "$objcopy" --strip-debug "target/$triple/release/ferry" /ferry

FROM gcr.io/distroless/cc-debian12:nonroot
LABEL org.opencontainers.image.source="https://github.com/ashmit27j/ferry" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.description="Ferry: a Kubernetes-native L7 load balancer and Gateway API implementation"
COPY --from=build /ferry /usr/local/bin/ferry
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/ferry"]
