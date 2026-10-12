//! HTTP proxying through an in-process Ferry against mock backends.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use ferry_config::SnapshotVersion;
use ferry_integration_tests::{
    TestProxy, any_port, dead_addr, get, send, single_cluster_yaml, snapshot,
};
use ferry_testkit::{Behavior, http as mock_http};

#[tokio::test]
async fn forwards_http1_and_preserves_host_and_query() {
    let backend = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&single_cluster_yaml(&[backend.addr()])).await;
    let reply = send(
        proxy.addr,
        "/a/b?x=1&y=2",
        &[("host", "Example.test")],
        false,
    )
    .await;
    assert_eq!(reply.status, 200);
    let echo = reply.echo();
    assert_eq!(echo["backend"], "a");
    assert_eq!(echo["path"], "/a/b");
    assert_eq!(echo["query"], "x=1&y=2");
    assert_eq!(echo["headers"]["host"], "Example.test");
    assert_eq!(echo["version"], "HTTP/1.1");
}

#[tokio::test]
async fn h2c_downstream_keeps_authority_as_host() {
    let backend = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&single_cluster_yaml(&[backend.addr()])).await;
    let reply = send(proxy.addr, "/h2", &[], true).await;
    assert_eq!(reply.status, 200);
    let echo = reply.echo();
    assert_eq!(echo["headers"]["host"], proxy.addr.to_string());
    assert_eq!(echo["version"], "HTTP/1.1", "upstream is HTTP/1.1 in M0");
}

#[tokio::test]
async fn round_robin_spreads_evenly() {
    let a = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let b = mock_http::spawn(any_port(), Behavior::named("b"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&single_cluster_yaml(&[a.addr(), b.addr()])).await;
    for _ in 0..10 {
        assert_eq!(get(proxy.addr, "/").await.status, 200);
    }
    assert_eq!(a.stats().requests(), 5);
    assert_eq!(b.stats().requests(), 5);
}

#[tokio::test]
async fn weighted_backends_follow_weights() {
    let stable = mock_http::spawn(any_port(), Behavior::named("stable"))
        .await
        .unwrap();
    let canary = mock_http::spawn(any_port(), Behavior::named("canary"))
        .await
        .unwrap();
    let yaml = format!(
        r#"
listeners:
  - name: http
    address: 127.0.0.1:0
    routes:
      - name: split
        rules:
          - backends: [{{ cluster: stable, weight: 3 }}, {{ cluster: canary, weight: 1 }}]
clusters:
  - {{ name: stable, endpoints: [{{ address: "{}" }}] }}
  - {{ name: canary, endpoints: [{{ address: "{}" }}] }}
"#,
        stable.addr(),
        canary.addr()
    );
    let proxy = TestProxy::start(&yaml).await;
    for _ in 0..8 {
        get(proxy.addr, "/").await;
    }
    assert_eq!(stable.stats().requests(), 6);
    assert_eq!(canary.stats().requests(), 2);
}

#[tokio::test]
async fn unmatched_host_gets_404_and_zero_weight_gets_500() {
    let backend = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let yaml = format!(
        r#"
listeners:
  - name: http
    address: 127.0.0.1:0
    routes:
      - name: only-foo
        hostnames: [foo.test]
        rules:
          - backends: [{{ cluster: c }}]
      - name: drained
        hostnames: [drained.test]
        rules:
          - backends: [{{ cluster: c, weight: 0 }}]
clusters:
  - {{ name: c, endpoints: [{{ address: "{}" }}] }}
"#,
        backend.addr()
    );
    let proxy = TestProxy::start(&yaml).await;
    assert_eq!(
        send(proxy.addr, "/", &[("host", "foo.test")], false)
            .await
            .status,
        200
    );
    assert_eq!(
        send(proxy.addr, "/", &[("host", "bar.test")], false)
            .await
            .status,
        404
    );
    assert_eq!(
        send(proxy.addr, "/", &[("host", "drained.test")], false)
            .await
            .status,
        500
    );
    assert_eq!(backend.stats().requests(), 1);
}

#[tokio::test]
async fn dead_backend_gets_502() {
    let proxy = TestProxy::start(&single_cluster_yaml(&[dead_addr()])).await;
    let reply = get(proxy.addr, "/").await;
    assert_eq!(reply.status, 502);
    assert_eq!(&reply.body[..], b"upstream unavailable");
}

#[tokio::test]
async fn hop_by_hop_headers_are_not_forwarded() {
    let backend = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&single_cluster_yaml(&[backend.addr()])).await;
    let reply = send(
        proxy.addr,
        "/",
        &[("connection", "x-hop"), ("x-hop", "1"), ("x-keep", "1")],
        false,
    )
    .await;
    let headers = &reply.echo()["headers"];
    assert!(headers.get("x-hop").is_none(), "{headers}");
    assert_eq!(headers["x-keep"], "1");
}

#[tokio::test]
async fn upstream_status_and_headers_pass_through() {
    let backend = mock_http::spawn(any_port(), Behavior::named("teapot"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&single_cluster_yaml(&[backend.addr()])).await;
    let reply = send(proxy.addr, "/", &[(mock_http::STATUS_HEADER, "418")], false).await;
    assert_eq!(reply.status, 418);
    assert_eq!(reply.headers[mock_http::BACKEND_HEADER], "teapot");
}

#[tokio::test]
async fn hot_swap_routes_new_requests_to_new_snapshot() {
    let a = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let b = mock_http::spawn(any_port(), Behavior::named("b"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&single_cluster_yaml(&[a.addr()])).await;
    assert_eq!(get(proxy.addr, "/").await.backend(), "a");

    proxy
        .reload
        .swap(snapshot(&single_cluster_yaml(&[b.addr()]), 2))
        .unwrap();
    assert_eq!(proxy.reload.current_version(), SnapshotVersion(2));
    assert_eq!(get(proxy.addr, "/").await.backend(), "b");
}

#[tokio::test]
async fn in_flight_request_finishes_on_its_original_snapshot() {
    let slow = Behavior::named("old").with_latency(Duration::from_millis(300));
    let old = mock_http::spawn(any_port(), slow).await.unwrap();
    let new = mock_http::spawn(any_port(), Behavior::named("new"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&single_cluster_yaml(&[old.addr()])).await;

    let addr = proxy.addr;
    let in_flight = tokio::spawn(async move { get(addr, "/").await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    proxy
        .reload
        .swap(snapshot(&single_cluster_yaml(&[new.addr()]), 2))
        .unwrap();

    assert_eq!(get(proxy.addr, "/").await.backend(), "new");
    let reply = in_flight.await.unwrap();
    assert_eq!(reply.status, 200);
    assert_eq!(reply.backend(), "old");
}

#[tokio::test]
async fn drain_completes_in_flight_requests_then_stops_accepting() {
    let slow = Behavior::named("slow").with_latency(Duration::from_millis(400));
    let backend = mock_http::spawn(any_port(), slow).await.unwrap();
    let mut proxy = TestProxy::start(&single_cluster_yaml(&[backend.addr()])).await;
    let addr = proxy.addr;

    let in_flight = tokio::spawn(async move { get(addr, "/").await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    proxy.begin_shutdown();

    let reply = in_flight.await.unwrap();
    assert_eq!(
        reply.status, 200,
        "in-flight request must complete during drain"
    );
    proxy.shutdown().await;
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}
