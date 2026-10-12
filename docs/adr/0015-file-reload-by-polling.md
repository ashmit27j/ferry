# ADR 0015: Reload file config by polling its content hash

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M1

## Context

Standalone mode (`ferry proxy --config file.yaml`) must hot-reload. File-event
APIs differ per OS (inotify, kqueue, ReadDirectoryChangesW), and miss the atomic
symlink swap Kubernetes uses for mounted ConfigMaps unless the parent directory
is watched. Configs are small (at most 4 MiB).

## Decision

`ferry_proxy::watch` re-reads the file every `--watch-interval-ms` (default
1,000; `0` disables) and reloads when the content hash changes. The new snapshot
gets the next version number. A file that cannot be read, parsed or validated is
logged once at `error` with every validation problem, and the proxy keeps
serving the last good snapshot.

## Consequences

- One small read per interval and no extra dependency; identical behaviour on
  every OS and with ConfigMap symlink swaps.
- Changes take effect within one interval (the exit test used 200 ms).
- In Kubernetes the controller pushes snapshots over gRPC (M4) and the watcher
  is not used.
