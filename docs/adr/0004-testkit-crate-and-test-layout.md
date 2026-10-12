# ADR 0004: Put mock backends in a ferry-testkit crate

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M0

## Context

The planned layout has no home for the mock backends (HTTP/1.1, h2c, gRPC,
WebSocket with latency and error injection). They are needed by unit-adjacent
tests in several crates, by `tests/`, by the binary's end-to-end test, and as a
standalone process for k6 load tests.

## Decision

Add `crates/ferry-testkit`: a library (`http`, `grpc`, `ws` modules sharing
`Behavior`, `Stats` and `MockServer`) plus a `mock-backend` binary. It is only
ever a dev-dependency of production crates.

Tests are layered:

| Layer | Location | What it runs |
| --- | --- | --- |
| Unit and property | `crates/*/src`, `crates/*/tests` | One crate, no sockets except testkit |
| Integration | `tests/` (`ferry-integration-tests`) | `ferry_proxy::Proxy` in-process against mocks |
| End to end | `bin/ferry/tests` | The real `ferry` binary as a child process |
| Cluster | `tests/e2e` (M4) | kind + Helm + Gateway API conformance |

Mocks bind port 0 and `MockServer::kill` cancels every connection task, so a
test can simulate a backend dying mid-request.

## Consequences

Test helpers may `unwrap`/`panic` (integration test crates carry a crate-level
`allow`); production crates may not. The mocks are themselves tested
(`crates/ferry-testkit/tests/mocks.rs`) so a proxy test failure is not a mock bug.
