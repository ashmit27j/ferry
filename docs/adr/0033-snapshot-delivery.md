# ADR 0033: Deliver snapshots as validated config JSON over mutual TLS

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M4

## Context

Proxies receive their configuration from the controller over the
`ferry.v1.SnapshotService` stream defined in M0 (ADR 0003). Snapshots carry
TLS private keys from Gateway certificate Secrets, so the channel must be
confidential and only proxies may read it. The plan asks for versioning,
ACK/NACK, and that a proxy keeps serving its last good snapshot when the
controller is unreachable. The human chose mutual TLS with certificates from
a `ferry certgen` Helm hook (the Envoy Gateway pattern).

## Decision

- **Payload:** `SnapshotUpdate.snapshot_json` is ferry-config's file format
  (`Config`) as JSON, certificates inline and rate limits already divided
  per replica. The proxy parses and validates it exactly like a config file,
  so the controller cannot push anything a file could not express.
- **Protocol:** the proxy's first message names its Gateway
  (`SnapshotAck.gateway`, added in M4). The controller sends that Gateway's
  current snapshot, then every change. The proxy ACKs (`error` empty) once
  the snapshot is swapped in, or NACKs with the reason and keeps the old one.
  Acks are matched by nonce.
- **Versions** start from wall-clock milliseconds and only increase within a
  replica, so a new leader continues above its predecessor. They are
  informational; a proxy applies whatever its replica sends.
- **Every replica serves streams.** Translation is deterministic, so leader
  and follower publish the same config for the same cluster state; proxies
  reach either through one ClusterIP Service. Streams end when a replica
  stops, so its graceful shutdown never waits on them; the proxy reconnects
  with jittered backoff (250 ms doubling to 10 s) and serves throughout.
- **mTLS:** `ferry certgen` creates a CA (3 years), a server certificate for
  the controller Service names and one client certificate shared by all
  proxies, in two `kubernetes.io/tls` Secrets. It runs as a pre-install and
  pre-upgrade hook and renews within 90 days of expiry. The controller
  requires a client certificate from that CA.
- **Version skew:** fields added to ferry-config after M3 skip serialising
  their defaults, so a proxy one release behind accepts snapshots that do not
  use them, and NACKs (keeping its last snapshot) those that do. Future
  fields must follow the same rule (tested in `ferry-config`).

## Consequences

- A compromised proxy pod can read every Gateway's snapshot (one shared
  client identity). Per-Gateway identities would need per-fleet certificates;
  revisit if proxies ever run in tenant namespaces.
- Certificates are read at start: after a renewal, restart the controller
  and proxies (`kubectl rollout restart`). Hot rotation is an M5 item.
- The JSON form costs more bytes than a typed protobuf message, which only
  matters at thousands of routes per Gateway (gRPC's limit is raised to
  64 MiB).
