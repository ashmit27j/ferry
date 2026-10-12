//! Retry policies and retry budgets.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::time::Duration;

use ferry_integration_tests::{TestProxy, any_port, get, request};
use ferry_testkit::{Behavior, MockServer, http as mock_http};

fn yaml(endpoints: &[SocketAddr], rule_extra: &str, cluster_extra: &str) -> String {
    let eps: Vec<String> = endpoints
        .iter()
        .map(|a| format!("{{ address: \"{a}\" }}"))
        .collect();
    format!(
        r"
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
    endpoints: [{}]
",
        eps.join(", ")
    )
}

async fn broken_and_healthy() -> (MockServer, MockServer) {
    let broken = mock_http::spawn(any_port(), Behavior::named("broken").with_errors(1.0, 503))
        .await
        .unwrap();
    let healthy = mock_http::spawn(any_port(), Behavior::named("healthy"))
        .await
        .unwrap();
    (broken, healthy)
}

#[tokio::test]
async fn policy_retries_5xx_on_another_endpoint() {
    let (broken, healthy) = broken_and_healthy().await;
    let proxy = TestProxy::start(&yaml(
        &[broken.addr(), healthy.addr()],
        "            retry: { attempts: 2, backoff: 1ms }",
        "",
    ))
    .await;
    for _ in 0..10 {
        let r = get(proxy.addr, "/").await;
        assert_eq!(r.status, 200);
        assert_eq!(r.backend(), "healthy");
    }
    assert!(
        broken.stats().requests() > 0,
        "round robin must have hit it"
    );
}

#[tokio::test]
async fn without_a_policy_5xx_passes_through() {
    let (broken, _healthy) = broken_and_healthy().await;
    let proxy = TestProxy::start(&yaml(&[broken.addr()], "", "")).await;
    assert_eq!(get(proxy.addr, "/").await.status, 503);
    assert_eq!(broken.stats().requests(), 1);
}

#[tokio::test]
async fn post_body_is_replayed_intact() {
    let (broken, healthy) = broken_and_healthy().await;
    let proxy = TestProxy::start(&yaml(
        &[broken.addr(), healthy.addr()],
        "            retry: { attempts: 1, codes: [503], backoff: 1ms }",
        "",
    ))
    .await;
    for _ in 0..4 {
        let r = request(proxy.addr, "POST", "/submit", &[], "hello, retry").await;
        assert_eq!(r.status, 200);
        assert_eq!(r.echo()["body_bytes"], 12);
    }
}

#[tokio::test]
async fn exhausted_budget_stops_retries() {
    let (broken, _) = broken_and_healthy().await;
    let proxy = TestProxy::start(&yaml(
        &[broken.addr()],
        "            retry: { attempts: 3, backoff: 1ms }",
        "    retry_budget: { budget_percent: 0, min_retries_per_second: 0 }",
    ))
    .await;
    for _ in 0..5 {
        assert_eq!(get(proxy.addr, "/").await.status, 503);
    }
    assert_eq!(
        broken.stats().requests(),
        5,
        "a zero budget allows no retries"
    );
}

#[tokio::test]
async fn budget_floor_caps_retries_on_a_quiet_cluster() {
    let (broken, _) = broken_and_healthy().await;
    let proxy = TestProxy::start(&yaml(
        &[broken.addr()],
        "            retry: { attempts: 3, backoff: 1ms }",
        "    retry_budget: { budget_percent: 0, min_retries_per_second: 1, ttl: 2s }",
    ))
    .await;
    for _ in 0..5 {
        assert_eq!(get(proxy.addr, "/").await.status, 503);
    }
    // 5 originals plus a floor of 1/s over a 2 s window.
    assert_eq!(broken.stats().requests(), 7);
}

#[tokio::test]
async fn attempt_timeout_is_retried_when_504_is_retriable() {
    let slow = mock_http::spawn(
        any_port(),
        Behavior::named("slow").with_latency(Duration::from_millis(600)),
    )
    .await
    .unwrap();
    let fast = mock_http::spawn(any_port(), Behavior::named("fast"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&yaml(
        &[slow.addr(), fast.addr()],
        "            timeouts: { backend_request: 100ms }\n            retry: { attempts: 1, codes: [504], backoff: 1ms }",
        "",
    ))
    .await;
    for _ in 0..4 {
        let r = get(proxy.addr, "/").await;
        assert_eq!(r.status, 200);
        assert_eq!(r.backend(), "fast");
    }
}
