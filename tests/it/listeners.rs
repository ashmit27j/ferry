//! Listeners that come and go with reloads (M4: Gateways change at runtime),
//! readiness before the first snapshot, and the Gateway API's local replies
//! for empty and invalid backends.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use ferry_integration_tests::{TestProxy, any_port, eventually, get, snapshot};
use ferry_proxy::{Drain, Proxy};
use ferry_telemetry::Telemetry;
use ferry_testkit::{Behavior, http as mock_http};

fn two_listeners(backend: std::net::SocketAddr, second: &str) -> String {
    format!(
        r#"
listeners:
  - name: first
    address: 127.0.0.1:0
    routes: [{{ name: r, rules: [{{ backends: [{{ cluster: c }}] }}] }}]
{second}
clusters:
  - {{ name: c, endpoints: [{{ address: "{backend}" }}] }}
"#
    )
}

#[tokio::test]
async fn reload_adds_and_removes_listeners() {
    let backend = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&two_listeners(backend.addr(), "")).await;
    assert_eq!(get(proxy.addr, "/").await.status, 200);
    assert!(proxy.reload.listener_addr("second").is_none());

    let second = r"
  - name: second
    address: 127.0.0.1:0
    routes: [{ name: r, rules: [{ backends: [{ cluster: c }] }] }]";
    proxy
        .reload
        .swap(snapshot(&two_listeners(backend.addr(), second), 2))
        .unwrap();
    let added = proxy
        .reload
        .listener_addr("second")
        .expect("bound on reload");
    assert_eq!(get(added, "/").await.status, 200, "new listener serves");
    assert_eq!(
        proxy.reload.listener_addr("first"),
        Some(proxy.addr),
        "unchanged listener keeps its socket"
    );

    proxy
        .reload
        .swap(snapshot(&two_listeners(backend.addr(), ""), 3))
        .unwrap();
    assert!(proxy.reload.listener_addr("second").is_none());
    eventually(Duration::from_secs(5), || {
        std::net::TcpStream::connect_timeout(&added, Duration::from_millis(200)).is_err()
    })
    .await;
    assert_eq!(
        get(proxy.addr, "/").await.status,
        200,
        "the rest keeps serving"
    );
    proxy.shutdown().await;
}

#[tokio::test]
async fn renamed_listener_keeps_its_socket() {
    let backend = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    // A fixed port: renaming must not need a second bind of the same address.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let yaml = |name: &str| {
        format!(
            r#"
listeners:
  - name: {name}
    address: 127.0.0.1:{port}
    routes: [{{ name: r, rules: [{{ backends: [{{ cluster: c }}] }}] }}]
clusters:
  - {{ name: c, endpoints: [{{ address: "{}" }}] }}
"#,
            backend.addr()
        )
    };
    let proxy = TestProxy::start(&yaml("old")).await;
    proxy.reload.swap(snapshot(&yaml("new"), 2)).unwrap();
    assert_eq!(proxy.reload.listener_addr("new"), Some(proxy.addr));
    assert_eq!(get(proxy.addr, "/").await.status, 200);
    proxy.shutdown().await;
}

#[tokio::test]
async fn failed_bind_rejects_the_reload() {
    let backend = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = TestProxy::start(&two_listeners(backend.addr(), "")).await;
    let second = format!(
        r"
  - name: second
    address: {}
    routes: [{{ name: r, rules: [{{ backends: [{{ cluster: c }}] }}] }}]",
        taken.local_addr().unwrap()
    );
    let err = proxy
        .reload
        .swap(snapshot(&two_listeners(backend.addr(), &second), 2))
        .unwrap_err();
    assert!(err.to_string().contains("cannot bind"), "{err}");
    assert_eq!(proxy.reload.current_version().0, 1, "old snapshot serves");
    assert_eq!(get(proxy.addr, "/").await.status, 200);
    proxy.shutdown().await;
}

#[tokio::test]
async fn awaiting_proxy_is_not_ready_until_the_first_snapshot() {
    let backend = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let proxy = Proxy::bind_awaiting(Arc::new(Telemetry::default()))
        .await
        .unwrap();
    let reload = proxy.reload_handle();
    let admin = tokio::net::TcpListener::bind(any_port()).await.unwrap();
    let admin_addr = admin.local_addr().unwrap();
    tokio::spawn(ferry_proxy::admin::serve(
        admin,
        reload.clone(),
        std::future::pending(),
    ));
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(proxy.run(
        async {
            let _ = stopped.await;
        },
        Drain::default(),
    ));
    let ready = get(admin_addr, "/ready").await;
    assert_eq!(ready.status, 503);
    assert!(reload.is_awaiting_config());

    reload
        .swap(snapshot(&two_listeners(backend.addr(), ""), 7))
        .unwrap();
    assert_eq!(get(admin_addr, "/ready").await.status, 200);
    let addr = reload.listener_addr("first").unwrap();
    assert_eq!(get(addr, "/").await.status, 200);
    let _ = stop.send(());
    task.await.unwrap().unwrap();
}

/// Gateway API: a Service with no ready endpoints answers 503, and the share
/// of invalid backend references answers 500.
#[tokio::test]
async fn empty_cluster_is_503_and_invalid_backends_are_500() {
    let yaml = r"
listeners:
  - name: http
    address: 127.0.0.1:0
    routes:
      - name: empty
        hostnames: [empty.test]
        rules: [{ backends: [{ cluster: none }] }]
      - name: invalid
        hostnames: [invalid.test]
        rules: [{ invalid_backend_weight: 1 }]
clusters:
  - { name: none, endpoints: [] }
";
    let proxy = TestProxy::start(yaml).await;
    let host = |h: &'static str| [("host", h)];
    let empty = ferry_integration_tests::send(proxy.addr, "/", &host("empty.test"), false).await;
    assert_eq!(empty.status, 503);
    let invalid =
        ferry_integration_tests::send(proxy.addr, "/", &host("invalid.test"), false).await;
    assert_eq!(invalid.status, 500);
    proxy.shutdown().await;
}
