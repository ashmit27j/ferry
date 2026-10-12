# ADR 0003: Compile protobufs with protox instead of protoc

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M0

## Context

`ferry-api` (snapshot stream) and `ferry-testkit` (gRPC echo service) need
generated tonic code. `tonic-prost-build` normally shells out to `protoc`, which
would be a system dependency on every developer machine, CI runner and container
build, with its own version drift.

## Decision

Each `build.rs` compiles `.proto` files with `protox` (a pure-Rust protobuf
compiler) into a `FileDescriptorSet` and hands that to
`tonic_prost_build::configure().compile_fds(...)`. Generated modules are wrapped
in `#[allow(missing_docs, clippy::all)]` because we do not control their output.

## Consequences

- `cargo build` is the only thing needed to build Ferry, on Linux, macOS and
  Windows.
- `protox` must track protobuf language features we use. We use plain proto3
  only, so this is low risk. Revisit if we need editions or custom options that
  `protox` does not support.
