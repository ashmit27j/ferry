# ADR 0006: Enforce the no-panic rule with workspace lints

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M0

## Context

Engineering rule 1 forbids `unwrap`, `expect` and `panic!` in library code and on
the request path; rule 2 forbids `unsafe`. Rules that rely on review alone erode.

## Decision

The workspace `[lints]` table, inherited by every crate, sets:

- `unsafe_code = "forbid"`.
- clippy `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`,
  `dbg_macro` = deny; `print_stdout`/`print_stderr` = warn.
- `missing_docs` and `missing_debug_implementations` = warn (errors under
  `clippy -D warnings` in CI).

`clippy.toml` exempts `#[test]` functions and `#[cfg(test)]` modules. Integration
test crates, benches and the testkit's test files opt out with a crate-level
`#![allow(...)]` because their helper functions are not `#[test]`s. Where a
production crate genuinely needs to print (the CLI), it uses a small function
with a local `#[allow]`.

We keep `panic = "unwind"` in release builds. A panic that slips through (in a
dependency, say) then kills one tokio task, which is one connection, instead of
the whole proxy. Hyper and tokio already isolate per-connection tasks.

## Consequences

Violations fail CI rather than review. Arithmetic overflow and slice indexing
can still panic; we use `get()`, checked or saturating arithmetic on the request
path, and the fuzz targets look for the rest. Revisit `clippy::indexing_slicing`
if fuzzing finds an indexing panic.
