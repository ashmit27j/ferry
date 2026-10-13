# Tools for the M4 exit run (bench/exit/run-m4.sh): kind, kubectl, helm, the
# docker CLI, and the Gateway API conformance suite compiled to one binary.
# Running them in a container keeps the run identical on Windows, macOS and
# Linux CI; the container joins the kind network to reach Gateway addresses.
FROM golang:1.26-bookworm

ARG KIND=v0.33.0
ARG KUBECTL=v1.37.1
ARG HELM=v3.19.0
ARG DOCKER=29.9.0
ARG GATEWAY_API=v1.6.3

RUN curl -fsSLo /usr/local/bin/kind "https://kind.sigs.k8s.io/dl/${KIND}/kind-linux-amd64" \
    && curl -fsSLo /usr/local/bin/kubectl "https://dl.k8s.io/release/${KUBECTL}/bin/linux/amd64/kubectl" \
    && curl -fsSL "https://get.helm.sh/helm-${HELM}-linux-amd64.tar.gz" | tar -xz -C /tmp \
    && mv /tmp/linux-amd64/helm /usr/local/bin/helm \
    && curl -fsSL "https://download.docker.com/linux/static/stable/x86_64/docker-${DOCKER}.tgz" | tar -xz -C /tmp \
    && mv /tmp/docker/docker /usr/local/bin/docker \
    && chmod +x /usr/local/bin/kind /usr/local/bin/kubectl \
    && rm -rf /tmp/*

RUN git clone --depth 1 --branch "${GATEWAY_API}" https://github.com/kubernetes-sigs/gateway-api.git /gateway-api \
    && cd /gateway-api \
    && go test -c -o /usr/local/bin/conformance.test ./conformance \
    && go clean -cache -modcache

WORKDIR /work
