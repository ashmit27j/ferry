//! Route rate limits.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;

use ferry_integration_tests::{TestProxy, any_port, get, send, snapshot};
use ferry_testkit::{Behavior, MockServer, http as mock_http};

fn yaml(backend: SocketAddr, limit: &str, cluster_extra: &str) -> String {
    format!(
        r#"
listeners:
  - name: http
    address: 127.0.0.1:0
    routes:
      - name: limited
        rate_limit: {limit}
        rules: [{{ backends: [{{ cluster: c }}] }}]
clusters:
  - name: c
{cluster_extra}
    endpoints: [{{ address: "{backend}" }}]
"#
    )
}

async fn backend() -> MockServer {
    mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap()
}

#[tokio::test]
async fn client_ip_bucket_answers_429_with_retry_after() {
    let b = backend().await;
    let proxy = TestProxy::start(&yaml(b.addr(), "{ requests: 3, per: 1m }", "")).await;
    for _ in 0..3 {
        assert_eq!(get(proxy.addr, "/").await.status, 200);
    }
    let limited = get(proxy.addr, "/").await;
    assert_eq!(limited.status, 429);
    assert_eq!(limited.headers["x-ferry-ratelimited"], "true");
    let retry_after: u64 = limited.headers["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=20).contains(&retry_after), "{retry_after}");
    assert_eq!(
        b.stats().requests(),
        3,
        "limited requests never reach the backend"
    );
}

#[tokio::test]
async fn header_keys_get_separate_buckets() {
    let b = backend().await;
    let proxy = TestProxy::start(&yaml(
        b.addr(),
        "{ requests: 1, per: 1m, key: { header: x-api-key } }",
        "",
    ))
    .await;
    let addr = proxy.addr;
    let with_key =
        |k: String| async move { send(addr, "/", &[("x-api-key", k.as_str())], false).await };
    // Keys hash into 4,096 slots with a random seed, so two keys share a
    // bucket now and then (ADR 0022). Ten fresh keys plus "no header": all
    // admitted bar the rare collision, never one key twice.
    let mut admitted = 0;
    for i in 0..10 {
        admitted += usize::from(with_key(format!("key-{i}")).await.status == 200);
    }
    admitted += usize::from(get(addr, "/").await.status == 200);
    assert!(admitted >= 9, "only {admitted} of 11 keys admitted");
    assert_eq!(with_key("key-0".into()).await.status, 429);
    // Requests without the header share one bucket.
    assert_eq!(get(addr, "/").await.status, 429);
}

#[tokio::test]
async fn buckets_survive_reloads_that_keep_the_limit() {
    let b = backend().await;
    let limit = "{ requests: 1, per: 1m }";
    let proxy = TestProxy::start(&yaml(b.addr(), limit, "")).await;
    assert_eq!(get(proxy.addr, "/").await.status, 200);
    assert_eq!(get(proxy.addr, "/").await.status, 429);

    // Unrelated change: the bucket is kept.
    proxy
        .reload
        .swap(snapshot(
            &yaml(b.addr(), limit, "    connect_timeout: 2s"),
            2,
        ))
        .unwrap();
    assert_eq!(get(proxy.addr, "/").await.status, 429);

    // A new limit starts fresh buckets.
    proxy
        .reload
        .swap(snapshot(
            &yaml(
                b.addr(),
                "{ requests: 2, per: 1m }",
                "    connect_timeout: 2s",
            ),
            3,
        ))
        .unwrap();
    assert_eq!(get(proxy.addr, "/").await.status, 200);
}
