# ADR 0002: Parse YAML with serde-saphyr, not serde_yaml

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M0

## Context

The plan lists `serde_yaml`. That crate is archived and published as
`0.9.34+deprecated`; its successor `serde_yml` is also deprecated. Depending on
either would trip `cargo deny` and `cargo audit` unmaintained checks, and the
config parser is a fuzz target that untrusted input (a mounted ConfigMap, a
`check-config` run in CI) can reach.

Candidates: `serde_norway` (a fork of `serde_yaml` on the old unsafe-libyaml
core), `serde_yaml_ng`, and `serde-saphyr` 1.3 (pure Rust on `saphyr-parser`,
advertised as panic-free, with built-in budgets against alias bombs and deep
nesting).

## Decision

Use `serde-saphyr` for both deserialisation and the serialiser used by property
tests. `ferry-config` wraps it behind `from_yaml_str`, `load_file` and
`load_snapshot`, and maps its errors to `ConfigError::Parse(String)` so the
dependency never appears in the public API. We also cap input at
`MAX_CONFIG_BYTES` (4 MiB) before parsing.

## Consequences

- The `config_yaml` fuzz target exercises a parser that was designed for hostile
  input, with its default budgets.
- Swapping the YAML crate later only touches `load.rs`.
- `serde-saphyr` is younger than `serde_yaml`; the nightly fuzz job is our check
  on it. Revisit if it is abandoned or fuzzing finds parser panics we cannot fix
  upstream.
