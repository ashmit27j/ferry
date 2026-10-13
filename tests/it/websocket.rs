//! WebSocket (HTTP/1.1 Upgrade) proxying.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;

use ferry_integration_tests::{TestProxy, any_port, get};
use ferry_testkit::{Behavior, http as mock_http, ws as mock_ws};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

fn yaml(backend: SocketAddr, protocol: &str) -> String {
    format!(
        r#"
listeners:
  - name: http
    address: 127.0.0.1:0
    routes:
      - name: all
        rules: [{{ backends: [{{ cluster: c }}] }}]
clusters:
  - {{ name: c, protocol: {protocol}, endpoints: [{{ address: "{backend}" }}] }}
"#
    )
}

#[tokio::test]
async fn websocket_messages_flow_both_ways() {
    let ws = mock_ws::spawn(any_port(), Behavior::named("ws"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&yaml(ws.addr(), "http1")).await;
    let (mut socket, resp) = tokio_tungstenite::connect_async(format!("ws://{}/chat", proxy.addr))
        .await
        .unwrap();
    assert_eq!(resp.status(), 101);
    assert!(resp.headers().contains_key("x-request-id"));

    socket.send(Message::text("hello")).await.unwrap();
    assert_eq!(
        socket.next().await.unwrap().unwrap(),
        Message::text("hello")
    );
    socket.send(Message::binary(vec![1u8, 2, 3])).await.unwrap();
    assert_eq!(
        socket.next().await.unwrap().unwrap(),
        Message::binary(vec![1u8, 2, 3])
    );
    socket.close(None).await.unwrap();
    assert_eq!(ws.stats().requests(), 2);
}

#[tokio::test]
async fn many_concurrent_websockets_each_get_their_own_tunnel() {
    let ws = mock_ws::spawn(any_port(), Behavior::named("ws"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&yaml(ws.addr(), "http1")).await;
    let addr = proxy.addr;
    let tasks: Vec<_> = (0..20)
        .map(|i| {
            tokio::spawn(async move {
                let (mut s, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
                    .await
                    .unwrap();
                for j in 0..5 {
                    let msg = format!("{i}-{j}");
                    s.send(Message::text(msg.clone())).await.unwrap();
                    assert_eq!(s.next().await.unwrap().unwrap(), Message::text(msg));
                }
                s.close(None).await.unwrap();
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(ws.stats().requests(), 100);
}

#[tokio::test]
async fn plain_requests_still_work_next_to_upgrades() {
    let http = mock_http::spawn(any_port(), Behavior::named("plain"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&yaml(http.addr(), "http1")).await;
    for _ in 0..3 {
        assert_eq!(get(proxy.addr, "/").await.status, 200);
    }
}

#[tokio::test]
async fn upgrade_to_an_http2_cluster_is_not_attempted() {
    // The h2c mock does not speak WebSocket; Ferry must not forward the
    // Upgrade, so the backend just sees a GET and answers it normally.
    let http = mock_http::spawn(any_port(), Behavior::named("h2"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&yaml(http.addr(), "http2")).await;
    let err = tokio_tungstenite::connect_async(format!("ws://{}/", proxy.addr)).await;
    assert!(err.is_err(), "no 101 without an HTTP/1.1 cluster");
    assert_eq!(http.stats().requests(), 1);
}
