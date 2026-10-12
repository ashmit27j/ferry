# ADR 0013: Freeze the public API of ferry-config

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M1

## Context

The plan freezes `ferry-config`'s public API at the end of M1. The controller
(M4) will build configs programmatically, and later milestones add fields (TLS,
timeouts, retries, rate limits). A freeze that blocked additions would block M2.

## Decision

The API as of this ADR is frozen. Its shape:

- **File model** (`Config` and friends): public fields, `#[non_exhaustive]`,
  `deny_unknown_fields`, with constructors (`Config::new`, `ClusterConfig::new`,
  `RuleConfig::forward`, …) for code outside the crate.
- **Snapshot** (`Snapshot`, `Listener`, `Route`, `Rule`, `RequestMatch`,
  `Filter`, `Cluster`, …): private fields and read-only accessors; enums are
  `#[non_exhaustive]`. Built only by `Config::into_snapshot`, which reports every
  error with a field path.
- **Loading:** `from_yaml_str`, `load_file`, `load_snapshot`, the 4 MiB cap.
- **Values:** `Duration` (GEP-2257 text such as `1m30s`), `SnapshotVersion`.

Allowed without a new ADR: new optional fields (with serde defaults), new enum
variants, new accessors and new constructors. Anything else needs an ADR that
supersedes this one: removing or renaming a field, changing a type, changing a
default, or tightening validation so that a previously valid config is rejected.

## Consequences

- Code outside the crate cannot use struct literals for model types and must
  use the constructors; the property tests and the controller do so.
- Matching on snapshot enums needs a wildcard arm, which keeps the data plane
  compiling as variants are added.
- `http` types (`HeaderName`, `HeaderValue`, `Method`) appear in the snapshot
  API, tying it to `http` 1.x, which is itself stable.
