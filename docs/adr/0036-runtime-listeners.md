# ADR 0036: Bind and release listeners on reload

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M4

## Context

Until M3 a proxy bound its listeners once; a reload that added, moved or
removed one logged "takes effect on restart" (an M2 known gap). Under the
controller, Gateway listeners change at any time (the conformance suite's
`GatewayModifyListeners` adds one), and a proxy started in Kubernetes has no
configuration until its first snapshot.

## Decision

- Listener sockets are **slots**, parallel to `Runtime::listeners`. A reload
  plans the change: a listener keeps its slot when its name and address are
  unchanged; a renamed listener on the same address takes over that slot's
  socket (a second bind could not); new listeners get new slots; listeners
  that left the config retire their slot. Slots are never reused, so a
  connection accepted on a removed listener can never be routed by a newer
  one.
- New sockets are bound **synchronously** (socket2, `SO_REUSEADDR` on Unix)
  before anything is published. A failed bind rejects the whole reload,
  which the controller sees as a NACK. Once the runtime is swapped, retired
  slots stop accepting and drain like a shutdown, and new sockets are handed
  to `Proxy::run`, which starts their accept loops.
- `Proxy::bind_awaiting` starts with an empty snapshot; `/ready` answers 503
  ("waiting for configuration") until the first successful swap.

## Consequences

- File-mode reloads now apply listener changes too.
- A listener moving to an address that overlaps one still bound by another
  slot (for example from `127.0.0.1:80` to `0.0.0.0:80` under a new name)
  fails to bind and is rejected; restart to apply it.
- Readiness gating means a proxy pod only joins its Service once it holds a
  configuration, which the controller's `Programmed` condition relies on.
