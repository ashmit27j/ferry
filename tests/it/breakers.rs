//! Circuit breakers: per-cluster limits that fail fast with 503.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::time::Duration;

use ferry_integration_tests::{TestProxy, any_port, get};
use ferry_testkit::{Behavior, http as mock_http};

fn yaml(backend: SocketAddr, cluster_extra: &str, rule_extra: &str) -> String {
    format!(
        r#"
listeners:
  - name: http
    address: 127.0.0.1:0
    routes:
      - name: all
        rules:
          - backends: [{{ cluster: c }}]
{rule_extra}
clusters:
  - name: c
{cluster_extra}
    endpoints: [{{ address: "{backend}" }}]
"#
    )
}

/// Sends `n` concurrent GETs and returns their statuses and overload flags.
async fn concurrent(addr: SocketAddr, n: usize) -> Vec<(u16, bool)> {
    let tasks: Vec<_> = (0..n)
        .map(|_| {
            tokio::spawn(async move {
                let r = get(addr, "/").await;
                (
                    r.status.as_u16(),
                    r.headers.contains_key("x-ferry-overloaded"),
                )
            })
        })
        .collect();
    let mut out = Vec::new();
    for t in tasks {
        out.push(t.await.unwrap());
    }
    out.sort_unstable();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_requests_fails_fast_with_overload_flag() {
    let b = mock_http::spawn(
        any_port(),
        Behavior::named("slow").with_latency(Duration::from_millis(400)),
    )
    .await
    .unwrap();
    let proxy = TestProxy::start(&yaml(
        b.addr(),
        "    circuit_breakers: { max_requests: 2 }",
        "",
    ))
    .await;
    let results = concurrent(proxy.addr, 6).await;
    let ok = results.iter().filter(|(s, _)| *s == 200).count();
    let shed: Vec<_> = results.iter().filter(|(s, _)| *s == 503).collect();
    assert_eq!(ok, 2, "{results:?}");
    assert_eq!(shed.len(), 4, "{results:?}");
    assert!(shed.iter().all(|(_, overloaded)| *overloaded));
    // Slots are released when responses finish.
    assert_eq!(get(proxy.addr, "/").await.status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_connections_limits_new_upstream_connections() {
    let b = mock_http::spawn(
        any_port(),
        Behavior::named("slow").with_latency(Duration::from_millis(400)),
    )
    .await
    .unwrap();
    let proxy = TestProxy::start(&yaml(
        b.addr(),
        "    circuit_breakers: { max_connections: 1 }",
        "",
    ))
    .await;
    let results = concurrent(proxy.addr, 3).await;
    assert_eq!(
        results.iter().filter(|(s, _)| *s == 200).count(),
        1,
        "{results:?}"
    );
    assert!(results.iter().any(|&(s, o)| s == 503 && o), "{results:?}");
    // The one connection is reused afterwards.
    assert_eq!(get(proxy.addr, "/").await.status, 200);
}

#[tokio::test]
async fn max_retries_zero_disables_policy_retries() {
    let b = mock_http::spawn(any_port(), Behavior::named("broken").with_errors(1.0, 503))
        .await
        .unwrap();
    let proxy = TestProxy::start(&yaml(
        b.addr(),
        "    circuit_breakers: { max_retries: 0 }",
        "            retry: { attempts: 3, backoff: 1ms }",
    ))
    .await;
    for _ in 0..3 {
        assert_eq!(get(proxy.addr, "/").await.status, 503);
    }
    assert_eq!(b.stats().requests(), 3);
}
