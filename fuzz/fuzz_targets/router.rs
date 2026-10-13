//! Fuzzes route matching with hostile request heads: arbitrary hosts (with
//! ports, odd case, IPv6 brackets), paths, methods, query strings and
//! headers against a table that uses every match type (exact, prefix and
//! regex paths, headers, query parameters, methods, wildcard hosts). Routing
//! must never panic, and a decision must name a route of the table.

#![no_main]

use std::sync::LazyLock;

use ferry_config::{Snapshot, SnapshotVersion, from_yaml_str};
use ferry_proxy::Router;
use ferry_proxy::router::RequestInfo;
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use libfuzzer_sys::fuzz_target;

const TABLE: &str = r#"
listeners:
  - name: http
    address: 127.0.0.1:0
    routes:
      - name: exact
        hostnames: [api.example.com]
        rules:
          - matches: [{ path: { type: Exact, value: /v1/users } }]
            backends: [{ cluster: c }]
      - name: prefix
        hostnames: ["*.example.com"]
        rules:
          - matches: [{ path: { type: PathPrefix, value: /static/ }, method: GET }]
            backends: [{ cluster: c }]
      - name: regex
        rules:
          - matches: [{ path: { type: RegularExpression, value: "/items/[0-9]+(/[a-z]+)?" } }]
            backends: [{ cluster: c }]
      - name: header
        rules:
          - matches:
              - headers: [{ name: x-canary, value: "true" }]
                query_params: [{ name: v, value: "2" }]
            backends: [{ cluster: c }]
      - name: header-regex
        rules:
          - matches: [{ headers: [{ type: RegularExpression, name: x-tenant, value: "t-[a-z]{2,8}" }] }]
            backends: [{ cluster: c }]
      - name: fallback
        rules: [{ backends: [{ cluster: c }] }]
clusters:
  - { name: c, endpoints: [{ address: "127.0.0.1:1" }] }
"#;

static SNAPSHOT: LazyLock<Snapshot> = LazyLock::new(|| {
    from_yaml_str(TABLE)
        .expect("fuzz route table parses")
        .into_snapshot(SnapshotVersion(1))
        .expect("fuzz route table validates")
});
static ROUTER: LazyLock<Router> = LazyLock::new(|| Router::new(&SNAPSHOT.listeners()[0]));

const METHODS: [&str; 6] = ["GET", "POST", "PUT", "DELETE", "PATCH", "CONNECT"];

fuzz_target!(|data: &[u8]| {
    // Fields split on NUL: method selector, host, path?query, then
    // name:value header lines.
    let mut fields = data.split(|&b| b == 0);
    let method_byte = fields.next().and_then(|f| f.first().copied()).unwrap_or(0);
    let method = Method::from_bytes(METHODS[usize::from(method_byte) % METHODS.len()].as_bytes())
        .expect("static method");
    let host = String::from_utf8_lossy(fields.next().unwrap_or_default());
    let target = String::from_utf8_lossy(fields.next().unwrap_or_default());
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (&*target, None),
    };
    let mut headers = HeaderMap::new();
    for line in fields.take(32) {
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(&line[..colon]),
            HeaderValue::from_bytes(&line[colon + 1..]),
        ) else {
            continue;
        };
        headers.append(name, value);
    }
    let req = RequestInfo {
        host: &host,
        method: &method,
        path,
        query,
        headers: &headers,
    };
    if let Some(d) = ROUTER.route(&req) {
        let names = ["exact", "prefix", "regex", "header", "header-regex", "fallback"];
        assert!(names.contains(&d.route), "unknown route {}", d.route);
    }
});
