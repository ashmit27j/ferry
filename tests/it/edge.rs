//! Edge behaviour: PROXY protocol, X-Forwarded-* and X-Request-Id.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::time::Duration;

use ferry_integration_tests::{TestProxy, any_port, send};
use ferry_testkit::{Behavior, MockServer, http as mock_http};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn setup(proxy_protocol: bool) -> (MockServer, TestProxy) {
    let backend = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let yaml = format!(
        r#"
listeners:
  - name: http
    address: 127.0.0.1:0
    proxy_protocol: {proxy_protocol}
    routes:
      - name: all
        rules:
          - backends: [{{ cluster: c }}]
clusters:
  - name: c
    lb_policy: maglev
    hash_policy: source_ip
    endpoints: [{{ address: "{}" }}]
"#,
        backend.addr()
    );
    (backend, TestProxy::start(&yaml).await)
}

async fn raw(addr: SocketAddr, bytes: &[u8]) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(bytes).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

fn echo_of(response: &str) -> serde_json::Value {
    let body = response.split("\r\n\r\n").nth(1).unwrap_or_default();
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {response}"))
}

const GET: &[u8] = b"GET /x HTTP/1.1\r\nHost: app.test\r\nX-Forwarded-For: 203.0.113.7\r\nConnection: close\r\n\r\n";

#[tokio::test]
async fn proxy_v1_header_sets_the_client_address() {
    let (_b, proxy) = setup(true).await;
    let mut bytes = b"PROXY TCP4 198.51.100.20 10.0.0.1 51000 80\r\n".to_vec();
    bytes.extend_from_slice(GET);
    let resp = raw(proxy.addr, &bytes).await;
    let echo = echo_of(&resp);
    assert_eq!(
        echo["headers"]["x-forwarded-for"],
        "203.0.113.7, 198.51.100.20"
    );
    assert_eq!(echo["headers"]["x-forwarded-proto"], "http");
}

#[tokio::test]
async fn proxy_v2_header_sets_the_client_address() {
    let (_b, proxy) = setup(true).await;
    let mut bytes = b"\r\n\r\n\0\r\nQUIT\n".to_vec();
    bytes.extend_from_slice(&[0x21, 0x11, 0x00, 0x0c]);
    bytes.extend_from_slice(&[192, 0, 2, 44, 10, 0, 0, 1]);
    bytes.extend_from_slice(&40000u16.to_be_bytes());
    bytes.extend_from_slice(&80u16.to_be_bytes());
    bytes.extend_from_slice(GET);
    let resp = raw(proxy.addr, &bytes).await;
    assert_eq!(
        echo_of(&resp)["headers"]["x-forwarded-for"],
        "203.0.113.7, 192.0.2.44"
    );
}

#[tokio::test]
async fn proxy_v2_local_command_uses_the_socket_peer() {
    let (_b, proxy) = setup(true).await;
    let mut bytes = b"\r\n\r\n\0\r\nQUIT\n".to_vec();
    bytes.extend_from_slice(&[0x20, 0x00, 0x00, 0x00]);
    bytes.extend_from_slice(GET);
    let resp = raw(proxy.addr, &bytes).await;
    assert_eq!(
        echo_of(&resp)["headers"]["x-forwarded-for"],
        "203.0.113.7, 127.0.0.1"
    );
}

#[tokio::test]
async fn missing_proxy_header_closes_the_connection() {
    let (backend, proxy) = setup(true).await;
    let resp = raw(proxy.addr, GET).await;
    assert!(resp.is_empty(), "{resp}");
    assert_eq!(backend.stats().requests(), 0);
}

#[tokio::test]
async fn without_proxy_protocol_the_socket_peer_is_the_client() {
    let (_b, proxy) = setup(false).await;
    let r = send(proxy.addr, "/", &[("x-request-id", "trace-42")], false).await;
    let echo = r.echo();
    assert_eq!(echo["headers"]["x-forwarded-for"], "127.0.0.1");
    assert_eq!(echo["headers"]["x-request-id"], "trace-42");
    assert_eq!(r.headers["x-request-id"], "trace-42");
}

#[tokio::test]
async fn request_ids_are_generated_and_echoed() {
    let (_b, proxy) = setup(false).await;
    let r = send(proxy.addr, "/", &[], false).await;
    let sent = r.echo()["headers"]["x-request-id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(sent.len(), 36);
    assert_eq!(r.headers["x-request-id"], sent.as_str());
}
