# ADR 0016: Request limits, HTTP/2 hardening and request-smuggling handling

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M2

## Context

The plan requires limits on header size and count, body size and HTTP/2
concurrent streams, mitigations for HTTP/2 Rapid Reset and CONTINUATION
floods, and rejection of request-smuggling patterns (conflicting
`Content-Length` and `Transfer-Encoding`). Ferry parses HTTP with hyper, so
whatever hyper decides before our code runs bounds what we can enforce.

## Decision

**Limits** (per listener, defaults from Envoy's edge guidance):

| Setting | Default | Mechanism |
| --- | --- | --- |
| `max_request_headers_kb` | 60 | HTTP/1.1 read buffer (431 when exceeded); HTTP/2 `SETTINGS_MAX_HEADER_LIST_SIZE` |
| `max_request_headers` | 100 | hyper's header count (431) |
| `max_request_body_bytes` | none | 413 before forwarding when `Content-Length` exceeds it; a streamed body that exceeds it is cut off and answered 413 |
| `h2_max_concurrent_streams` | 100 | `SETTINGS_MAX_CONCURRENT_STREAMS` |

HTTP/2 also gets Envoy's flow-control windows (64 KiB per stream, 1 MiB per
connection) and keep-alive pings.

**HTTP/2 attacks.**
- Rapid Reset (CVE-2023-44487): at most 20 streams opened and reset by the
  client before the server sees them, then GOAWAY `ENHANCE_YOUR_CALM`.
- MadeYouReset (CVE-2025-8671): at most 1,024 resets we send because of client
  errors, then the connection is closed.
- CONTINUATION floods: the `h2` crate stops decoding a header block once it
  passes `max_header_list_size` and closes the connection, so floods are bounded
  by the header limit.

**Smuggling.**
- Conflicting `Content-Length` values: rejected with 400 (hyper).
- A `Transfer-Encoding` that does not end in `chunked`, or any
  `Transfer-Encoding` on HTTP/1.0: rejected with 400 (hyper).
- Both `Content-Length` and `Transfer-Encoding`: hyper applies RFC 9112 §6.1.
  It decodes by `Transfer-Encoding`, drops `Content-Length`, and closes the
  connection after the response. Ferry then strips `Transfer-Encoding` (it is
  hop-by-hop) and frames the body itself towards the backend. hyper does not
  expose the dropped header, so Ferry cannot answer 400 here as Envoy does;
  supporting that would mean replacing hyper's HTTP/1.1 parser.

## Consequences

- The attack the plan names, a front end and back end disagreeing on message
  boundaries, cannot happen: the downstream connection is not reused, and the
  backend only ever sees Ferry's framing. `cl_te_smuggling_never_reaches_the_backend`
  checks both header orders.
- The behaviour differs from the plan's literal "reject": the client gets the
  response to the decoded request rather than 400. Revisit if hyper gains a
  strict mode, or if a compliance requirement demands 400.
- Limits come from the listener config and apply to new connections after a
  reload; open connections keep the settings they started with.
